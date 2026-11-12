package org.k1.kdesk

import java.io.DataInputStream
import java.io.IOException
import java.nio.ByteBuffer

/**
 * kDesk wire protocol, mirror of `crates/kdesk-proto/src/lib.rs`.
 * Keep the two in sync; the layout is documented at the top of that file.
 *
 * Frame: kind u8 | payload length u32 big-endian | payload. Integers in
 * payloads are big-endian too (which is also ByteBuffer's default).
 */
object Protocol {
    const val VERSION = 1
    const val DEFAULT_PORT = 7420

    const val HEADER_LEN = 5
    const val MAX_PAYLOAD = 16 * 1024 * 1024

    const val KIND_HELLO = 0x01
    const val KIND_CONFIG = 0x02
    const val KIND_VIDEO = 0x03
    const val KIND_TOUCH = 0x10
    const val KIND_PING = 0x20
    const val KIND_PONG = 0x21
    const val KIND_REQUEST_KEYFRAME = 0x30

    const val CODEC_H264 = 1

    const val FLAG_KEYFRAME = 0b01
    const val FLAG_CONFIG = 0b10

    const val TOUCH_DOWN = 0
    const val TOUCH_MOVE = 1
    const val TOUCH_UP = 2
    const val TOUCH_CANCEL = 3

    // ---- client -> host -------------------------------------------------

    fun hello(width: Int, height: Int, name: String): ByteArray {
        val nameBytes = name.toByteArray(Charsets.UTF_8)
        val b = ByteBuffer.allocate(10 + nameBytes.size)
        b.putShort(VERSION.toShort())
        b.putInt(width)
        b.putInt(height)
        b.put(nameBytes)
        return frame(KIND_HELLO, b.array())
    }

    /** [x] and [y] are normalised 0..65535 over the video. */
    fun touch(action: Int, pointerId: Int, x: Int, y: Int): ByteArray {
        val b = ByteBuffer.allocate(6)
        b.put(action.toByte())
        b.put(pointerId.toByte())
        b.putShort(x.coerceIn(0, 65535).toShort())
        b.putShort(y.coerceIn(0, 65535).toShort())
        return frame(KIND_TOUCH, b.array())
    }

    fun pong(token: Long): ByteArray =
        frame(KIND_PONG, ByteBuffer.allocate(8).putLong(token).array())

    fun requestKeyframe(): ByteArray = frame(KIND_REQUEST_KEYFRAME, ByteArray(0))

    private fun frame(kind: Int, payload: ByteArray): ByteArray {
        val b = ByteBuffer.allocate(HEADER_LEN + payload.size)
        b.put(kind.toByte())
        b.putInt(payload.size)
        b.put(payload)
        return b.array()
    }

    // ---- host -> client -------------------------------------------------

    /**
     * Read one message. Blocks. Throws [IOException] on EOF or a corrupt
     * stream. Unknown kinds are skipped by returning [HostMessage.Ignored].
     */
    fun read(input: DataInputStream): HostMessage {
        val kind = input.readUnsignedByte()
        val len = input.readInt()
        if (len < 0 || len > MAX_PAYLOAD) throw IOException("bad payload length $len")
        val payload = ByteArray(len)
        input.readFully(payload)
        return decode(kind, payload)
    }

    fun decode(kind: Int, payload: ByteArray): HostMessage {
        val b = ByteBuffer.wrap(payload)
        return when (kind) {
            KIND_CONFIG -> {
                need(kind, payload, 11)
                val version = b.getShort().toInt() and 0xffff
                if (version != VERSION) {
                    throw IOException("host speaks protocol v$version, this app speaks v$VERSION")
                }
                HostMessage.Config(width = b.getInt(), height = b.getInt(), codec = b.get().toInt() and 0xff)
            }
            KIND_VIDEO -> {
                need(kind, payload, 9)
                val pts = b.getLong()
                val flags = b.get().toInt() and 0xff
                // The access unit is payload[9..]; keep the array, skip the copy.
                HostMessage.Video(ptsUs = pts, flags = flags, data = payload, offset = 9)
            }
            KIND_PING -> {
                need(kind, payload, 8)
                HostMessage.Ping(b.getLong())
            }
            KIND_PONG -> {
                need(kind, payload, 8)
                HostMessage.Pong(b.getLong())
            }
            else -> HostMessage.Ignored(kind)
        }
    }

    private fun need(kind: Int, payload: ByteArray, n: Int) {
        if (payload.size < n) {
            throw IOException("message 0x${kind.toString(16)} too short: ${payload.size} < $n")
        }
    }
}

sealed interface HostMessage {
    data class Config(val width: Int, val height: Int, val codec: Int) : HostMessage

    /** Annex-B H.264 access unit at `data[offset until data.size]`. */
    class Video(val ptsUs: Long, val flags: Int, val data: ByteArray, val offset: Int) : HostMessage {
        val isKeyframe: Boolean get() = flags and Protocol.FLAG_KEYFRAME != 0
        val size: Int get() = data.size - offset
    }

    data class Ping(val token: Long) : HostMessage
    data class Pong(val token: Long) : HostMessage
    data class Ignored(val kind: Int) : HostMessage
}
