package org.k1.kdesk

import java.io.BufferedInputStream
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.LinkedBlockingQueue

/**
 * Connects to the PC through `adb reverse` (the host listens on its own
 * localhost, the phone dials its own localhost) and keeps reconnecting until
 * [stop] is called.
 *
 * All [Listener] callbacks arrive on the connection's reader thread, not the
 * main thread.
 */
class HostConnection(
    private val port: Int,
    private val deviceName: String,
    private val screenWidth: Int,
    private val screenHeight: Int,
    private val listener: Listener,
) {
    interface Listener {
        fun onStatus(text: String)
        fun onConfig(width: Int, height: Int)
        fun onVideo(msg: HostMessage.Video)
        fun onDisconnected()
    }

    private val outgoing = LinkedBlockingQueue<ByteArray>()
    private var current: Run? = null

    @Synchronized
    fun start() {
        if (current != null) return
        current = Run().also { it.start() }
    }

    @Synchronized
    fun stop() {
        current?.shutdown()
        current = null
    }

    /**
     * Queue an already-framed message (see [Protocol]). Safe from any thread.
     * [droppable] messages (touch moves) are discarded if the writer is far
     * behind, so a stalled link never builds up a backlog of stale positions.
     */
    fun send(frame: ByteArray, droppable: Boolean = false) {
        if (droppable && outgoing.size > MAX_DROPPABLE_BACKLOG) return
        outgoing.offer(frame)
    }

    /** One start()..stop() lifetime. A fresh instance per start avoids races with a slow-dying old thread. */
    private inner class Run : Thread("kdesk-net") {
        @Volatile private var active = true
        @Volatile private var socket: Socket? = null

        init {
            isDaemon = true
        }

        fun shutdown() {
            active = false
            closeQuietly()
            interrupt()
        }

        private fun closeQuietly() {
            try {
                socket?.close()
            } catch (_: IOException) {
            }
        }

        override fun run() {
            while (active) {
                var gotConfig = false
                var writer: Thread? = null
                try {
                    listener.onStatus("Connecting to the PC...")
                    val s = Socket()
                    socket = s
                    s.tcpNoDelay = true
                    s.connect(InetSocketAddress("127.0.0.1", port), CONNECT_TIMEOUT_MS)
                    if (!active) throw IOException("stopped")

                    outgoing.clear()
                    writer = Thread({ writeLoop(s.getOutputStream()) }, "kdesk-tx").also {
                        it.isDaemon = true
                        it.start()
                    }
                    outgoing.offer(Protocol.hello(screenWidth, screenHeight, deviceName))
                    listener.onStatus("Connected.\nApprove the virtual screen on your PC if it asks.")

                    val input = DataInputStream(BufferedInputStream(s.getInputStream(), 64 * 1024))
                    while (active) {
                        when (val m = Protocol.read(input)) {
                            is HostMessage.Config -> {
                                if (m.codec != Protocol.CODEC_H264) {
                                    throw IOException("host sent an unsupported codec (${m.codec})")
                                }
                                gotConfig = true
                                listener.onConfig(m.width, m.height)
                            }
                            is HostMessage.Video -> listener.onVideo(m)
                            is HostMessage.Ping -> send(Protocol.pong(m.token))
                            else -> Unit
                        }
                    }
                } catch (e: IOException) {
                    if (active) listener.onStatus(explain(e, gotConfig))
                } finally {
                    closeQuietly()
                    writer?.interrupt()
                    listener.onDisconnected()
                }

                if (active) {
                    try {
                        sleep(RECONNECT_DELAY_MS)
                    } catch (_: InterruptedException) {
                        return
                    }
                }
            }
        }

        private fun explain(e: IOException, gotConfig: Boolean): String {
            val msg = e.message.orEmpty()
            return when {
                msg.startsWith("host speaks protocol") || msg.startsWith("host sent") -> msg
                gotConfig -> "Connection lost. Reconnecting..."
                else ->
                    "Can't reach the PC.\n\nOn the PC run:\nadb reverse tcp:$port tcp:$port\nthen start kdesk-host."
            }
        }

        private fun writeLoop(out: OutputStream) {
            try {
                while (true) {
                    out.write(outgoing.take())
                    // Batch whatever else is already waiting into one flush.
                    while (true) {
                        out.write(outgoing.poll() ?: break)
                    }
                    out.flush()
                }
            } catch (_: Exception) {
                // Socket closed or thread interrupted: the reader notices and reconnects.
            }
        }
    }

    private companion object {
        const val CONNECT_TIMEOUT_MS = 2000
        const val RECONNECT_DELAY_MS = 1000L
        const val MAX_DROPPABLE_BACKLOG = 256
    }
}
