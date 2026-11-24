package com.thunderlight.remote.display

import android.content.Context
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import java.nio.ByteBuffer
import kotlin.math.max
import kotlin.math.min

private const val TAG = "Streamer"
private const val AUDIO_RATE = 48_000
private const val AUDIO_CHANNELS = 2

private const val AUDIO_STREAMING = 1
private const val AUDIO_UNSUPPORTED = 2
private const val AUDIO_DENIED = 3
private const val AUDIO_FAILED = 4

/**
 * Android-side plumbing around the native core: owns the MediaProjection + VirtualDisplay,
 * follows display rotation/size changes, and captures playback audio. Encoding, framing and the
 * network all happen in C++ ([NativeCore]).
 */
class Streamer(
    private val ctx: Context,
    private val projection: MediaProjection,
    private val maxSize: Int,
    private val bitrate: Int,
) {
    private val dm = ctx.getSystemService(DisplayManager::class.java)
    private val worker = HandlerThread("streamer").apply { start() }
    private val handler = Handler(worker.looper)

    private var virtualDisplay: VirtualDisplay? = null
    private var dpi = 320
    private var lastGeometry: Triple<Int, Int, Int>? = null // w, h, rotation
    @Volatile private var closed = false

    private var audioThread: Thread? = null
    private var audioRecord: AudioRecord? = null

    private val bridge = object : NativeCore.Bridge {
        override fun onTouch(action: Int, x: Int, y: Int, frameW: Int, frameH: Int) {
            InputService.instance?.onTouch(action, x, y, frameW, frameH)
        }

        override fun onScroll(x: Int, y: Int, frameW: Int, frameH: Int, dx: Int, dy: Int) {
            InputService.instance?.onScroll(x, y, frameW, frameH, dx, dy)
        }

        override fun onNav(action: Int) {
            InputService.instance?.onNav(action)
        }

        override fun onText(utf8: ByteArray) {
            InputService.instance?.onText(String(utf8, Charsets.UTF_8))
        }

        override fun onKey(down: Boolean, keycode: Int) {
            InputService.instance?.onKey(down, keycode)
        }

        override fun inputReady(): Boolean = InputService.instance != null
    }

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(id: Int) {}
        override fun onDisplayRemoved(id: Int) {}
        override fun onDisplayChanged(id: Int) {
            if (id == Display.DEFAULT_DISPLAY) handler.post { applyGeometry() }
        }
    }

    fun start() {
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.i(TAG, "projection stopped by system")
                stop()
                CaptureService.requestStop(ctx)
            }

            // Android 14+: the captured area changed size (e.g. the captured content rotated).
            override fun onCapturedContentResize(width: Int, height: Int) {
                Log.i(TAG, "captured content resized to ${width}x$height")
                handler.post { applyGeometry() }
            }
        }, handler)

        check(NativeCore.nativeStart(bridge, BuildConfig_VERSION_CODE, Build.VERSION.SDK_INT, Build.MODEL)) { "native core failed to start" }

        val (w, h, rot) = computeGeometry()
        lastGeometry = Triple(w, h, rot)
        val surface = checkNotNull(NativeCore.nativeSwitchEncoder(w, h, rot, bitrate)) { "native encoder failed to start" }
        virtualDisplay = projection.createVirtualDisplay(
            "remote-display", w, h, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, surface, null, handler,
        )
        dm.registerDisplayListener(displayListener, handler)
        startAudio()
    }

    // ---- video -------------------------------------------------------------------------------

    private fun computeGeometry(): Triple<Int, Int, Int> {
        val d = dm.getDisplay(Display.DEFAULT_DISPLAY)
        val m = DisplayMetrics()
        @Suppress("DEPRECATION") d.getRealMetrics(m)
        dpi = m.densityDpi
        val scale = min(1f, maxSize.toFloat() / max(m.widthPixels, m.heightPixels))
        val w = (m.widthPixels * scale).toInt() and 7.inv()
        val h = (m.heightPixels * scale).toInt() and 7.inv()
        return Triple(max(w, 16), max(h, 16), d.rotation)
    }

    /** Display changed (rotation, fold, resolution switch): new encoder at the new size. */
    private fun applyGeometry() {
        if (closed) return
        val g = computeGeometry()
        if (g == lastGeometry) return
        Log.i(TAG, "geometry $lastGeometry -> $g")
        lastGeometry = g
        val surface = NativeCore.nativeSwitchEncoder(g.first, g.second, g.third, bitrate) ?: return
        virtualDisplay?.resize(g.first, g.second, dpi)
        virtualDisplay?.surface = surface
        NativeCore.nativeReleaseRetiredEncoder()
    }

    // ---- audio -------------------------------------------------------------------------------

    private fun startAudio() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            NativeCore.nativeSetAudio(AUDIO_RATE, AUDIO_CHANNELS, AUDIO_UNSUPPORTED)
            return
        }
        if (ctx.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) != android.content.pm.PackageManager.PERMISSION_GRANTED) {
            NativeCore.nativeSetAudio(AUDIO_RATE, AUDIO_CHANNELS, AUDIO_DENIED)
            return
        }
        try {
            audioRecord = buildAudioRecord().also { it.startRecording() }
            NativeCore.nativeSetAudio(AUDIO_RATE, AUDIO_CHANNELS, AUDIO_STREAMING)
            audioThread = Thread(::audioLoop, "audio-capture").also { it.start() }
        } catch (e: Exception) {
            Log.w(TAG, "audio capture unavailable", e)
            NativeCore.nativeSetAudio(AUDIO_RATE, AUDIO_CHANNELS, AUDIO_FAILED)
        }
    }

    @android.annotation.TargetApi(Build.VERSION_CODES.Q)
    private fun buildAudioRecord(): AudioRecord {
        val cfg = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val fmt = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(AUDIO_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val min = AudioRecord.getMinBufferSize(AUDIO_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
        return AudioRecord.Builder()
            .setAudioPlaybackCaptureConfig(cfg)
            .setAudioFormat(fmt)
            .setBufferSizeInBytes(max(min, AUDIO_RATE * AUDIO_CHANNELS * 2 / 10))
            .build()
    }

    /** Reads 20 ms chunks into one reused direct buffer; native code sends it in place. */
    private fun audioLoop() {
        val rec = audioRecord ?: return
        val chunk = AUDIO_RATE / 50 * AUDIO_CHANNELS * 2
        val buf = ByteBuffer.allocateDirect(chunk)
        val t0 = System.nanoTime()
        while (!closed) {
            var n = 0
            while (n < chunk && !closed) {
                buf.limit(chunk).position(n)
                val r = rec.read(buf, chunk - n)
                if (r < 0) return
                n += r
            }
            if (n > 0) NativeCore.nativeSendAudio(buf, n, (System.nanoTime() - t0) / 1000)
        }
    }

    fun stop() {
        if (closed) return
        closed = true
        dm.unregisterDisplayListener(displayListener)
        try { audioRecord?.stop() } catch (_: Exception) {}
        audioRecord?.release()
        virtualDisplay?.release()
        NativeCore.nativeStop()
        try { projection.stop() } catch (_: Exception) {}
        worker.quitSafely()
    }
}

/** Mirrors versionCode in app/build.gradle.kts (BuildConfig is disabled to keep the build minimal). */
const val BuildConfig_VERSION_CODE = 9
