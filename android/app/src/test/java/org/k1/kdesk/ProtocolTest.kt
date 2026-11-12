package org.k1.kdesk

import java.io.ByteArrayInputStream
import java.io.DataInputStream
import java.io.IOException
import java.nio.ByteBuffer
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/** These bytes are the same ones the Rust side asserts in kdesk-proto's tests. */
class ProtocolTest {

    private fun bytes(vararg v: Int) = ByteArray(v.size) { v[it].toByte() }

    @Test
    fun headerLayoutMatchesHost() {
        // Rust: Message::RequestKeyframe.encode() == [0x30, 0, 0, 0, 0]
        assertArrayEquals(bytes(0x30, 0, 0, 0, 0), Protocol.requestKeyframe())
        // Rust: Message::Ping(1).encode() == [0x20, 0,0,0,8, 0,0,0,0,0,0,0,1]
        assertArrayEquals(
            bytes(0x21, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1),
            Protocol.pong(1),
        )
    }

    @Test
    fun touchIsBigEndianAndClamped() {
        val f = Protocol.touch(Protocol.TOUCH_MOVE, pointerId = 3, x = 65535, y = -5)
        assertArrayEquals(bytes(0x10, 0, 0, 0, 6, 1, 3, 0xff, 0xff, 0, 0), f)
    }

    @Test
    fun helloLayout() {
        val f = Protocol.hello(2400, 1080, "Pixel")
        val b = ByteBuffer.wrap(f)
        assertEquals(Protocol.KIND_HELLO.toByte(), b.get())
        assertEquals(10 + 5, b.getInt())
        assertEquals(Protocol.VERSION.toShort(), b.getShort())
        assertEquals(2400, b.getInt())
        assertEquals(1080, b.getInt())
        assertEquals("Pixel", String(f, 15, 5, Charsets.UTF_8))
    }

    @Test
    fun decodesConfig() {
        // version 1, 1920x1080, codec 1
        val payload = ByteBuffer.allocate(11).putShort(1).putInt(1920).putInt(1080).put(1).array()
        val framed = bytes(0x02, 0, 0, 0, 11) + payload
        val msg = Protocol.read(DataInputStream(ByteArrayInputStream(framed)))
        assertEquals(HostMessage.Config(1920, 1080, 1), msg)
    }

    @Test
    fun decodesVideoWithoutCopyingPayload() {
        val au = bytes(0, 0, 0, 1, 0x65, 9, 9)
        val payload = ByteBuffer.allocate(9 + au.size).putLong(123456789L).put(0b11).put(au).array()
        val msg = Protocol.decode(Protocol.KIND_VIDEO, payload) as HostMessage.Video
        assertEquals(123456789L, msg.ptsUs)
        assertTrue(msg.isKeyframe)
        assertEquals(au.size, msg.size)
        assertArrayEquals(au, msg.data.copyOfRange(msg.offset, msg.data.size))
    }

    @Test
    fun rejectsOversizedAndShortAndWrongVersion() {
        val huge = bytes(0x03, 0x7f, 0xff, 0xff, 0xff)
        try {
            Protocol.read(DataInputStream(ByteArrayInputStream(huge)))
            fail("expected IOException")
        } catch (_: IOException) {
        }

        try {
            Protocol.decode(Protocol.KIND_VIDEO, ByteArray(3))
            fail("expected IOException")
        } catch (_: IOException) {
        }

        val wrong = ByteBuffer.allocate(11).putShort(99).putInt(1).putInt(1).put(1).array()
        try {
            Protocol.decode(Protocol.KIND_CONFIG, wrong)
            fail("expected IOException")
        } catch (_: IOException) {
        }
    }

    @Test
    fun unknownKindIsIgnored() {
        assertEquals(HostMessage.Ignored(0x7f), Protocol.decode(0x7f, ByteArray(0)))
    }
}
