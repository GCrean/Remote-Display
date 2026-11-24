package com.thunderlight.remote.display

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.accessibilityservice.GestureDescription.StrokeDescription
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.DisplayMetrics
import android.view.Display
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.hardware.display.DisplayManager
import kotlin.math.abs
import kotlin.math.max
import kotlin.math.min

/**
 * Injects input from the desktop without root: touches/scrolls via gesture dispatch,
 * navigation via global actions, text via accessibility set-text on the focused field.
 */
class InputService : AccessibilityService() {
    companion object {
        @Volatile var instance: InputService? = null
    }

    private val main = Handler(Looper.getMainLooper())
    private val HOLD_MS = 350L
    private val SLOP = 12f

    // Touch stroke state (main thread only).
    private var stroke: StrokeDescription? = null
    private var lastX = 0f
    private var lastY = 0f
    private var inFlight = false
    private var pendX = 0f
    private var pendY = 0f
    private var hasPend = false
    private var pendUp = false

    // Scroll accumulation (main thread only).
    private var scrollX = 0f
    private var scrollY = 0f
    private var scrollDx = 0f
    private var scrollDy = 0f
    private var scrolling = false

    override fun onServiceConnected() {
        instance = this
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        instance = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        instance = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}

    // Entry points, called from the native core's client thread via NativeCore.Bridge.

    fun onTouch(action: Int, x: Int, y: Int, fw: Int, fh: Int) {
        val (sx, sy) = scale(x, y, fw, fh)
        main.post { touch(action, sx, sy) }
    }

    fun onScroll(x: Int, y: Int, fw: Int, fh: Int, dx: Int, dy: Int) {
        val (sx, sy) = scale(x, y, fw, fh)
        val m = realMetrics()
        val rx = m.widthPixels.toFloat() / max(fw, 1)
        val ry = m.heightPixels.toFloat() / max(fh, 1)
        main.post { scroll(sx, sy, dx * rx * 3, dy * ry * 3) }
    }

    fun onNav(action: Int) {
        val a = when (action) {
            1 -> GLOBAL_ACTION_BACK
            2 -> GLOBAL_ACTION_HOME
            3 -> GLOBAL_ACTION_RECENTS
            else -> GLOBAL_ACTION_NOTIFICATIONS
        }
        main.post { performGlobalAction(a) }
    }

    fun onText(s: String) {
        main.post { insertText(s) }
    }

    fun onKey(down: Boolean, code: Int) {
        if (down) main.post {
            when (code) {
                67 -> deleteBack()
                66 -> enter()
            }
        }
    }

    private fun realMetrics(): DisplayMetrics {
        val d = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
        val m = DisplayMetrics()
        @Suppress("DEPRECATION") d.getRealMetrics(m)
        return m
    }

    /** Video-frame pixels -> real display pixels (frame may be downscaled or briefly stale after rotation). */
    private fun scale(x: Int, y: Int, fw: Int, fh: Int): Pair<Float, Float> {
        val m = realMetrics()
        val sx = x.toFloat() * m.widthPixels / max(fw, 1)
        val sy = y.toFloat() * m.heightPixels / max(fh, 1)
        return min(max(sx, 0f), m.widthPixels - 1f) to min(max(sy, 0f), m.heightPixels - 1f)
    }

    // ---- touch -------------------------------------------------------------------------------

    private fun path(x0: Float, y0: Float, x1: Float, y1: Float) = Path().apply { moveTo(x0, y0); lineTo(x1, y1) }

    private var downActive = false
    private var downX = 0f
    private var downY = 0f
    private val holdRunnable = Runnable { if (downActive && stroke == null) beginStroke() }

    /**
     * A finger-down is not dispatched immediately: dispatch latency would turn every click into
     * a long-press. Quick press/release becomes a single tap stroke; movement past the slop, or a
     * hold of HOLD_MS, starts a stroke that stays down until released.
     */
    private fun touch(action: Int, x: Float, y: Float) {
        when (action) {
            0 -> {
                main.removeCallbacks(holdRunnable)
                stroke = null; hasPend = false; pendUp = false
                downActive = true; downX = x; downY = y
                main.postDelayed(holdRunnable, HOLD_MS)
            }
            1 -> if (downActive) {
                pendX = x; pendY = y; hasPend = true
                if (stroke == null) {
                    if (abs(x - downX) < SLOP && abs(y - downY) < SLOP) { hasPend = false; return }
                    main.removeCallbacks(holdRunnable)
                    beginStroke()
                }
                flush()
            }
            else -> if (downActive) {
                main.removeCallbacks(holdRunnable)
                downActive = false
                if (stroke == null) {
                    taps.add(floatArrayOf(downX, downY)); pump()
                } else {
                    pendX = x; pendY = y; hasPend = true; pendUp = true; flush()
                }
            }
        }
    }

    // Taps waiting for the gesture pipeline. A new gesture cancels the one in flight, so quick
    // consecutive clicks must be dispatched one after another, never on top of each other.
    private val taps = ArrayDeque<FloatArray>()

    private fun pump() {
        if (inFlight || stroke != null) return
        val t = taps.removeFirstOrNull() ?: return
        dispatch(StrokeDescription(path(t[0], t[1], t[0], t[1]), 0, 30, false), null)
    }

    private fun beginStroke() {
        if (inFlight || taps.isNotEmpty()) {  // let earlier taps finish first
            main.postDelayed(holdRunnable, 10)
            return
        }
        lastX = downX; lastY = downY
        val s = StrokeDescription(path(downX, downY, downX, downY), 0, 1, true)
        stroke = s
        dispatch(s, s)
    }

    /** [owner] is the held stroke this dispatch continues (null for a standalone tap). */
    private fun dispatch(s: StrokeDescription, owner: StrokeDescription?) {
        inFlight = true
        val ok = dispatchGesture(
            GestureDescription.Builder().addStroke(s).build(),
            object : GestureResultCallback() {
                override fun onCompleted(g: GestureDescription?) { main.post { inFlight = false; flush(); pump() } }
                override fun onCancelled(g: GestureDescription?) {
                    android.util.Log.w("Input", "gesture cancelled")
                    main.post {
                        inFlight = false
                        if (owner != null && stroke === owner) { stroke = null; hasPend = false; pendUp = false }
                        pump()
                    }
                }
            },
            null,
        )
        if (!ok) {
            android.util.Log.w("Input", "dispatchGesture returned false")
            inFlight = false
            if (owner != null) stroke = null
            pump()
        }
    }

    private fun flush() {
        val s = stroke ?: return
        if (inFlight) return
        if (pendUp) {
            val n = s.continueStroke(path(lastX, lastY, pendX, pendY), 0, 8, false)
            stroke = null; pendUp = false; hasPend = false
            dispatch(n, null)
        } else if (hasPend) {
            val n = s.continueStroke(path(lastX, lastY, pendX, pendY), 0, 12, true)
            lastX = pendX; lastY = pendY; hasPend = false
            stroke = n
            dispatch(n, n)
        }
    }

    // ---- scroll ------------------------------------------------------------------------------

    private fun scroll(x: Float, y: Float, dx: Float, dy: Float) {
        scrollX = x; scrollY = y; scrollDx += dx; scrollDy += dy
        flushScroll()
    }

    private fun flushScroll() {
        if (scrolling || (abs(scrollDx) < 8 && abs(scrollDy) < 8)) return
        val m = realMetrics()
        val dx = scrollDx.coerceIn(-600f, 600f); val dy = scrollDy.coerceIn(-600f, 600f)
        scrollDx = 0f; scrollDy = 0f
        val x1 = (scrollX + dx).coerceIn(0f, m.widthPixels - 1f)
        val y1 = (scrollY + dy).coerceIn(0f, m.heightPixels - 1f)
        scrolling = true
        val ok = dispatchGesture(
            GestureDescription.Builder().addStroke(StrokeDescription(path(scrollX, scrollY, x1, y1), 0, 120)).build(),
            object : GestureResultCallback() {
                override fun onCompleted(g: GestureDescription?) { main.post { scrolling = false; flushScroll() } }
                override fun onCancelled(g: GestureDescription?) { main.post { scrolling = false } }
            },
            null,
        )
        if (!ok) scrolling = false
    }

    // ---- text --------------------------------------------------------------------------------

    private fun findNode(n: AccessibilityNodeInfo?, depth: Int, pred: (AccessibilityNodeInfo) -> Boolean): AccessibilityNodeInfo? {
        if (n == null || depth > 40) return null
        if (pred(n)) return n
        for (i in 0 until n.childCount) findNode(n.getChild(i), depth + 1, pred)?.let { return it }
        return null
    }

    /**
     * The text field that has input focus. findFocus() alone comes back empty for some UI toolkits
     * (e.g. Jetpack Compose text fields), so fall back to walking the app windows' node trees.
     */
    private fun focusedField(): AccessibilityNodeInfo? {
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable }?.let { return it }
        val roots = ArrayList<AccessibilityNodeInfo>()
        rootInActiveWindow?.let { roots.add(it) }
        for (w in windows) if (w.type == android.view.accessibility.AccessibilityWindowInfo.TYPE_APPLICATION) w.root?.let { roots.add(it) }
        for (r in roots) findNode(r, 0) { it.isEditable && it.isFocused }?.let { return it }
        for (r in roots) findNode(r, 0) { it.isEditable && it.isAccessibilityFocused }?.let { return it }
        android.util.Log.w("Input", "no focused editable field found (${roots.size} roots)")
        return null
    }

    private fun currentText(n: AccessibilityNodeInfo): String =
        if (Build.VERSION.SDK_INT >= 26 && n.isShowingHintText) "" else n.text?.toString() ?: ""

    // SET_TEXT is applied asynchronously by the target app, so reading the field back between two
    // quick keystrokes returns stale text and the second edit overwrites the first. After each edit
    // we remember what the field should now contain and keep editing that copy while typing is
    // in progress; it is discarded once typing pauses or the focused field changes.
    private var shadowNode: AccessibilityNodeInfo? = null
    private var shadowText = ""
    private var shadowCursor = 0
    private var shadowAt = 0L
    private val SHADOW_MS = 1000L

    /** Current (text, selStart, selEnd) of [n], preferring the not-yet-applied shadow copy. */
    private fun state(n: AccessibilityNodeInfo): Triple<String, Int, Int> {
        val sh = shadowNode
        if (sh != null && sh == n && SystemClock.uptimeMillis() - shadowAt < SHADOW_MS) {
            return Triple(shadowText, shadowCursor, shadowCursor)
        }
        val t = currentText(n)
        val (a, b) = selection(n, t.length)
        return Triple(t, a, b)
    }

    private fun replace(n: AccessibilityNodeInfo, text: String, cursor: Int) {
        shadowNode = n; shadowText = text; shadowCursor = cursor; shadowAt = SystemClock.uptimeMillis()
        n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
        })
        n.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, cursor)
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, cursor)
        })
    }

    private fun selection(n: AccessibilityNodeInfo, len: Int): Pair<Int, Int> {
        val a = n.textSelectionStart; val b = n.textSelectionEnd
        if (a < 0 || b < 0) return len to len
        return min(min(a, b), len) to min(max(a, b), len)
    }

    private fun insertText(s: String) {
        val n = focusedField() ?: return
        val (t, a, b) = state(n)
        replace(n, t.substring(0, a) + s + t.substring(b), a + s.length)
    }

    private fun deleteBack() {
        val n = focusedField() ?: return
        val (t, a, b) = state(n)
        if (a != b) replace(n, t.substring(0, a) + t.substring(b), a)
        else if (a > 0) replace(n, t.substring(0, a - 1) + t.substring(a), a - 1)
    }

    private fun enter() {
        val n = focusedField() ?: return
        if (Build.VERSION.SDK_INT >= 30 && n.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)) return
        insertText("\n")
    }
}
