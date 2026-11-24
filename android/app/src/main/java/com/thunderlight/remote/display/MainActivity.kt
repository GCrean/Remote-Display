package com.thunderlight.remote.display

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.drawable.GradientDrawable
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.view.LayoutInflater
import android.view.View
import android.view.WindowInsets
import android.widget.LinearLayout
import android.widget.TextView

/**
 * Launched by the desktop app (`am start --ez autostart true`) to obtain MediaProjection consent,
 * or manually by the user. Starts [CaptureService] once consent is granted, and shows what is
 * (and is not) set up.
 */
class MainActivity : Activity() {
    private val reqCapture = 1
    private val reqPerms = 2

    private var autostart = false
    private var maxSize = 1920
    private var bitrate = 8_000_000
    /** A permission request started by "Start sharing" continues into screen-capture consent. */
    private var continueToCapture = false

    private val ui = Handler(Looper.getMainLooper())
    private val ticker = object : Runnable {
        override fun run() {
            refresh()
            ui.postDelayed(this, 1000)
        }
    }

    private lateinit var statusDot: View
    private lateinit var statusTitle: TextView
    private lateinit var statusDetail: TextView
    private lateinit var btnStart: TextView
    private lateinit var btnStop: TextView
    private lateinit var sharingRow: Row
    private lateinit var inputRow: Row
    private lateinit var audioRow: Row

    private class Row(val dot: View, val subtitle: TextView, val action: TextView)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Draw behind the system bars on every API level (Android 15 enforces this anyway); the
        // insets listener below keeps the content clear of them.
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false)
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
                View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
        }
        setContentView(R.layout.activity_main)

        statusDot = findViewById(R.id.status_dot)
        statusTitle = findViewById(R.id.status_title)
        statusDetail = findViewById(R.id.status_detail)
        btnStart = findViewById(R.id.btn_start)
        btnStop = findViewById(R.id.btn_stop)

        val checks = findViewById<LinearLayout>(R.id.checks)
        sharingRow = addRow(checks, "Screen sharing", first = true)
        inputRow = addRow(checks, "Remote input")
        audioRow = addRow(checks, "Audio capture")

        inputRow.action.text = "Enable"
        inputRow.action.setOnClickListener { startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)) }
        audioRow.action.text = "Grant"
        audioRow.action.setOnClickListener {
            continueToCapture = false
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), reqPerms)
        }
        btnStart.setOnClickListener { begin() }
        btnStop.setOnClickListener {
            CaptureService.requestStop(this)
            ui.postDelayed({ refresh() }, 300)
        }

        // Edge-to-edge: the gradient header runs under the status bar; keep content clear of the bars.
        val header = findViewById<View>(R.id.header)
        val content = findViewById<View>(R.id.content)
        val headerTop = header.paddingTop
        findViewById<View>(R.id.root).setOnApplyWindowInsetsListener { _, insets ->
            @Suppress("DEPRECATION")
            val top = insets.systemWindowInsetTop
            @Suppress("DEPRECATION")
            val bottom = insets.systemWindowInsetBottom
            header.setPadding(header.paddingLeft, headerTop + top, header.paddingRight, header.paddingBottom)
            content.setPadding(0, 0, 0, bottom)
            WindowInsets.CONSUMED
        }

        handleIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        ui.post(ticker)
    }

    override fun onPause() {
        ui.removeCallbacks(ticker)
        super.onPause()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    private fun addRow(parent: LinearLayout, title: String, first: Boolean = false): Row {
        if (!first) {
            parent.addView(View(this).apply {
                setBackgroundColor(getColor(R.color.stroke))
                layoutParams = LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 1)
            })
        }
        val v = LayoutInflater.from(this).inflate(R.layout.item_check, parent, false)
        v.findViewById<TextView>(R.id.title).text = title
        parent.addView(v)
        return Row(v.findViewById(R.id.dot), v.findViewById(R.id.subtitle), v.findViewById(R.id.action))
    }

    private fun handleIntent(i: Intent) {
        maxSize = i.getIntExtra("max_size", maxSize)
        bitrate = i.getIntExtra("bitrate", bitrate)
        autostart = i.getBooleanExtra("autostart", false)
        refresh()
        if (autostart) begin()
    }

    private fun setDot(v: View, colorRes: Int) {
        val d = (v.background.mutate() as GradientDrawable)
        d.setColor(getColor(colorRes))
        v.background = d
    }

    private fun refresh() {
        val sharing = CaptureService.running
        val inputOn = InputService.instance != null
        val audioGranted = checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        val audioSupported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

        setDot(statusDot, if (sharing) R.color.ok else R.color.off)
        statusTitle.text = if (sharing) "Sharing is active" else "Not sharing"
        statusDetail.text = if (sharing) {
            "Your computer can see and control this screen. Close the desktop app or press Stop to end it."
        } else {
            "Press Start sharing, or connect from the desktop app. Android will ask you to allow screen capture."
        }

        setDot(sharingRow.dot, if (sharing) R.color.ok else R.color.off)
        sharingRow.subtitle.text = if (sharing) "Streaming to the desktop app" else "Off"

        setDot(inputRow.dot, if (inputOn) R.color.ok else R.color.warn)
        inputRow.subtitle.text = if (inputOn) "Accessibility service enabled" else "Turn it on to tap, scroll and type from the desktop"
        inputRow.action.visibility = if (inputOn) View.GONE else View.VISIBLE

        when {
            !audioSupported -> {
                setDot(audioRow.dot, R.color.off)
                audioRow.subtitle.text = "Desktop audio needs Android 10 or newer"
                audioRow.action.visibility = View.GONE
            }
            audioGranted -> {
                setDot(audioRow.dot, R.color.ok)
                audioRow.subtitle.text = "Permission granted"
                audioRow.action.visibility = View.GONE
            }
            else -> {
                setDot(audioRow.dot, R.color.warn)
                audioRow.subtitle.text = "Permission needed to play this phone's sound on the desktop"
                audioRow.action.visibility = View.VISIBLE
            }
        }

        btnStart.alpha = if (sharing) 0.45f else 1f
        btnStart.isEnabled = !sharing
        btnStop.visibility = if (sharing) View.VISIBLE else View.GONE
    }

    private fun begin() {
        val needed = buildList {
            if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) add(Manifest.permission.RECORD_AUDIO)
            if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) add(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (needed.isNotEmpty()) {
            continueToCapture = true
            requestPermissions(needed.toTypedArray(), reqPerms)
        } else {
            requestCapture()
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        // Audio is optional: continue either way; the host is told if audio is unavailable.
        if (requestCode == reqPerms && continueToCapture) requestCapture()
        continueToCapture = false
        refresh()
    }

    private fun requestCapture() {
        val mgr = getSystemService(MediaProjectionManager::class.java)
        // Android 14+ lets the user share a single app instead of the screen. We mirror and control the
        // whole display (coordinates, rotation, system UI), so ask for the default display only.
        val intent = if (Build.VERSION.SDK_INT >= 34) {
            mgr.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            mgr.createScreenCaptureIntent()
        }
        @Suppress("DEPRECATION")
        startActivityForResult(intent, reqCapture)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode != reqCapture) return
        if (resultCode != RESULT_OK || data == null) {
            statusTitle.text = "Screen capture was declined"
            statusDetail.text = "Sharing needs your permission. Press Start sharing to try again."
            setDot(statusDot, R.color.danger)
            return
        }
        startForegroundService(
            Intent(this, CaptureService::class.java)
                .putExtra(CaptureService.EXTRA_RESULT_CODE, resultCode)
                .putExtra(CaptureService.EXTRA_RESULT_DATA, data)
                .putExtra(CaptureService.EXTRA_MAX_SIZE, maxSize)
                .putExtra(CaptureService.EXTRA_BITRATE, bitrate),
        )
        // Step aside so the user can use the device; sharing continues in the service.
        if (autostart) moveTaskToBack(true) else ui.postDelayed({ refresh() }, 500)
    }
}
