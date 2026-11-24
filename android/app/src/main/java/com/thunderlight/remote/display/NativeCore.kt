package com.thunderlight.remote.display

import android.view.Surface
import java.nio.ByteBuffer

/**
 * JNI surface of `librdcore` (src/main/cpp): the socket server, wire protocol, per-client
 * send queue and the H.264 encoder loop all run natively. Kotlin only supplies what Android
 * exposes through Java alone: MediaProjection, playback-capture audio and input injection.
 */
object NativeCore {
    init {
        System.loadLibrary("rdcore")
    }

    /** Called by native code on its own threads when the host sends input. */
    interface Bridge {
        fun onTouch(action: Int, x: Int, y: Int, frameW: Int, frameH: Int)
        fun onScroll(x: Int, y: Int, frameW: Int, frameH: Int, dx: Int, dy: Int)
        fun onNav(action: Int)
        fun onText(utf8: ByteArray)
        fun onKey(down: Boolean, keycode: Int)
        fun inputReady(): Boolean
    }

    external fun nativeStart(bridge: Bridge, versionCode: Int, sdk: Int, model: String): Boolean

    /** Starts an encoder at this size and returns its input Surface; the previous one keeps running. */
    external fun nativeSwitchEncoder(width: Int, height: Int, rotation: Int, bitrate: Int): Surface?
    external fun nativeReleaseRetiredEncoder()

    external fun nativeSetAudio(rate: Int, channels: Int, status: Int)

    /** [buffer] must be a direct ByteBuffer (read in place by native code). */
    external fun nativeSendAudio(buffer: ByteBuffer, len: Int, ptsUs: Long)

    external fun nativeStop()
}
