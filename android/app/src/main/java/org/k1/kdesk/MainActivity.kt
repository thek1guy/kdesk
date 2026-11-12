package org.k1.kdesk

import android.app.Activity
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * Full-screen video of the PC's virtual monitor, with touch sent back.
 *
 * Touch coordinates are normalised over the *video rectangle* (the surface is
 * letterboxed to the stream's aspect ratio), so a touch lands where the finger
 * is on the picture no matter the phone's own screen shape.
 */
class MainActivity : Activity(), SurfaceHolder.Callback, HostConnection.Listener {

    private val main = Handler(Looper.getMainLooper())

    /** Guards [surface], [videoW], [videoH] and decoder start/stop. */
    private val lock = Any()
    private var surface: Surface? = null
    private var videoW = 0
    private var videoH = 0

    private lateinit var root: FrameLayout
    private lateinit var surfaceView: SurfaceView
    private lateinit var statusView: TextView
    private lateinit var connection: HostConnection
    private lateinit var decoder: VideoDecoder

    /** Main thread only. True while video is on screen. */
    private var streaming = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        surfaceView = SurfaceView(this)
        root.addView(
            surfaceView,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        statusView = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 18f
            gravity = Gravity.CENTER
            setPadding(48, 48, 48, 48)
            text = "kDesk"
        }
        root.addView(
            statusView,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
                Gravity.CENTER,
            ),
        )
        setContentView(root)

        surfaceView.holder.addCallback(this)
        surfaceView.setOnTouchListener { v, ev ->
            onVideoTouch(v, ev)
            true
        }
        root.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fitSurface() }

        // The host should see the phone's real screen, landscape-first.
        val dm = resources.displayMetrics
        connection = HostConnection(
            port = Protocol.DEFAULT_PORT,
            deviceName = Build.MODEL ?: "Android",
            screenWidth = max(dm.widthPixels, dm.heightPixels),
            screenHeight = min(dm.widthPixels, dm.heightPixels),
            listener = this,
        )
        decoder = VideoDecoder(
            onNeedKeyframe = { connection.send(Protocol.requestKeyframe()) },
            onError = { msg ->
                main.post {
                    showStatus("Decoder error:\n$msg")
                    main.postDelayed({ restartDecoderAfterError() }, 1000)
                }
            },
        )

        hideSystemBars()
    }

    override fun onStart() {
        super.onStart()
        showStatus("Connecting to the PC...")
        connection.start()
    }

    override fun onStop() {
        connection.stop()
        synchronized(lock) {
            decoder.stop()
            videoW = 0
            videoH = 0
        }
        super.onStop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    // ---- SurfaceHolder.Callback (main thread) --------------------------------

    override fun surfaceCreated(holder: SurfaceHolder) {
        synchronized(lock) {
            surface = holder.surface
            startDecoderLocked()
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        synchronized(lock) {
            decoder.stop()
            surface = null
        }
    }

    // ---- HostConnection.Listener (network thread) ----------------------------

    override fun onStatus(text: String) {
        main.post { showStatus(text) }
    }

    override fun onConfig(width: Int, height: Int) {
        synchronized(lock) {
            decoder.stop()
            videoW = width
            videoH = height
            startDecoderLocked()
        }
        main.post {
            streaming = true
            statusView.visibility = View.GONE
            fitSurface()
        }
    }

    override fun onVideo(msg: HostMessage.Video) {
        decoder.submit(msg)
    }

    override fun onDisconnected() {
        synchronized(lock) {
            decoder.stop()
            videoW = 0
            videoH = 0
        }
    }

    // ---- helpers -------------------------------------------------------------

    /** Caller holds [lock]. */
    private fun startDecoderLocked() {
        val s = surface
        if (s != null && s.isValid && videoW > 0 && videoH > 0) {
            decoder.start(s, videoW, videoH)
        }
    }

    private fun restartDecoderAfterError() {
        synchronized(lock) {
            if (!decoder.isRunning) startDecoderLocked()
        }
        if (decoder.isRunning) {
            streaming = true
            statusView.visibility = View.GONE
        }
    }

    private fun showStatus(text: String) {
        streaming = false
        statusView.text = text
        statusView.visibility = View.VISIBLE
    }

    /** Letterbox the surface to the stream's aspect ratio, centred. */
    private fun fitSurface() {
        val vw: Int
        val vh: Int
        synchronized(lock) {
            vw = videoW
            vh = videoH
        }
        val cw = root.width
        val ch = root.height
        if (vw <= 0 || vh <= 0 || cw <= 0 || ch <= 0) return

        val scale = min(cw.toFloat() / vw, ch.toFloat() / vh)
        val w = (vw * scale).roundToInt()
        val h = (vh * scale).roundToInt()
        val lp = surfaceView.layoutParams as FrameLayout.LayoutParams
        if (lp.width != w || lp.height != h || lp.gravity != Gravity.CENTER) {
            lp.width = w
            lp.height = h
            lp.gravity = Gravity.CENTER
            surfaceView.layoutParams = lp
        }
    }

    private fun onVideoTouch(v: View, ev: MotionEvent) {
        if (!streaming) return
        val w = max(v.width, 1).toFloat()
        val h = max(v.height, 1).toFloat()

        fun norm(p: Float, size: Float) = ((p / size).coerceIn(0f, 1f) * 65535f).roundToInt()
        fun emit(action: Int, i: Int) {
            connection.send(
                Protocol.touch(action, ev.getPointerId(i), norm(ev.getX(i), w), norm(ev.getY(i), h)),
                droppable = action == Protocol.TOUCH_MOVE,
            )
        }

        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> emit(Protocol.TOUCH_DOWN, ev.actionIndex)
            MotionEvent.ACTION_MOVE -> for (i in 0 until ev.pointerCount) emit(Protocol.TOUCH_MOVE, i)
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> emit(Protocol.TOUCH_UP, ev.actionIndex)
            MotionEvent.ACTION_CANCEL -> for (i in 0 until ev.pointerCount) emit(Protocol.TOUCH_CANCEL, i)
        }
    }

    @Suppress("DEPRECATION")
    private fun hideSystemBars() {
        if (Build.VERSION.SDK_INT >= 28) {
            val lp = window.attributes
            lp.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
            window.attributes = lp
        }
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false)
            window.insetsController?.let {
                it.hide(WindowInsets.Type.systemBars())
                it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        } else {
            window.decorView.systemUiVisibility = (
                View.SYSTEM_UI_FLAG_FULLSCREEN or
                    View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
                    View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
                    View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                    View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                )
        }
    }
}
