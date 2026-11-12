package org.k1.kdesk

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * H.264 (Annex-B access units) -> Surface, tuned for latency.
 *
 * Frames come in on the network thread via [submit] and are decoded on a
 * dedicated thread. The decoder only ever starts on a keyframe, and if it
 * falls behind it drops everything and asks the host for a fresh keyframe
 * ([onNeedKeyframe]) rather than building up lag.
 */
class VideoDecoder(
    private val onNeedKeyframe: () -> Unit,
    private val onError: (String) -> Unit,
) {
    private val queue = ArrayBlockingQueue<HostMessage.Video>(QUEUE_DEPTH)

    @Volatile private var thread: Thread? = null
    @Volatile private var running = false
    @Volatile private var waitingForKey = true

    val isRunning: Boolean get() = running

    @Synchronized
    fun start(surface: Surface, width: Int, height: Int) {
        stop()
        queue.clear()
        waitingForKey = true
        running = true
        val t = Thread({ decodeLoop(surface, width, height) }, "kdesk-decode")
        thread = t
        t.start()
        // We're about to start mid-stream: get a clean IDR to begin on.
        onNeedKeyframe()
    }

    @Synchronized
    fun stop() {
        running = false
        val t = thread ?: return
        thread = null
        t.interrupt()
        try {
            t.join(1000)
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
        }
    }

    /** Called from the network thread for every video message. */
    fun submit(msg: HostMessage.Video) {
        if (!running || msg.size == 0) return
        if (waitingForKey) {
            if (!msg.isKeyframe) return
            waitingForKey = false
        }
        if (!queue.offer(msg)) {
            resync()
        }
    }

    /** Throw away everything queued and wait for the next keyframe. */
    private fun resync() {
        queue.clear()
        waitingForKey = true
        onNeedKeyframe()
    }

    private fun decodeLoop(surface: Surface, width: Int, height: Int) {
        var codec: MediaCodec? = null
        try {
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
            if (Build.VERSION.SDK_INT >= 30) {
                format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
            format.setInteger(MediaFormat.KEY_PRIORITY, 0) // 0 = realtime

            val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            codec = c
            c.configure(format, surface, null, 0)
            c.start()

            val info = MediaCodec.BufferInfo()
            while (running) {
                val frame = queue.poll(POLL_MS, TimeUnit.MILLISECONDS)
                if (frame != null) {
                    val index = c.dequeueInputBuffer(INPUT_TIMEOUT_US)
                    if (index < 0) {
                        // Decoder is not taking input: we're behind.
                        resync()
                    } else {
                        val buf = c.getInputBuffer(index)
                        if (buf == null || frame.size > buf.capacity()) {
                            c.queueInputBuffer(index, 0, 0, 0, 0)
                            resync()
                        } else {
                            buf.clear()
                            buf.put(frame.data, frame.offset, frame.size)
                            c.queueInputBuffer(index, 0, frame.size, frame.ptsUs, 0)
                        }
                    }
                }
                drain(c, info)
            }
        } catch (e: InterruptedException) {
            // stop() asked us to quit.
        } catch (e: Exception) {
            if (running) {
                Log.e(TAG, "decoder failed", e)
                if (thread === Thread.currentThread()) running = false
                onError(e.message ?: e.javaClass.simpleName)
            }
        } finally {
            try {
                codec?.stop()
            } catch (_: Exception) {
            }
            try {
                codec?.release()
            } catch (_: Exception) {
            }
        }
    }

    /** Show every decoded frame immediately; nothing waits for a presentation time. */
    private fun drain(codec: MediaCodec, info: MediaCodec.BufferInfo) {
        while (true) {
            val out = codec.dequeueOutputBuffer(info, 0)
            when {
                out >= 0 -> codec.releaseOutputBuffer(out, true)
                out == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> Unit
                else -> return
            }
        }
    }

    private companion object {
        const val TAG = "kdesk"
        const val QUEUE_DEPTH = 6
        const val POLL_MS = 5L
        const val INPUT_TIMEOUT_US = 20_000L
    }
}
