package com.thunderlight.remote.display

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder

/** Foreground service (type mediaProjection) hosting the [Streamer]. */
class CaptureService : Service() {
    companion object {
        const val EXTRA_RESULT_CODE = "result_code"
        const val EXTRA_RESULT_DATA = "result_data"
        const val EXTRA_MAX_SIZE = "max_size"
        const val EXTRA_BITRATE = "bitrate"
        private const val ACTION_STOP = "stop"
        private const val CHANNEL = "capture"

        @Volatile var running = false

        fun requestStop(ctx: Context) {
            ctx.startService(Intent(ctx, CaptureService::class.java).setAction(ACTION_STOP))
        }
    }

    private var streamer: Streamer? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            shutdown()
            return START_NOT_STICKY
        }
        if (intent == null || streamer != null) return START_NOT_STICKY

        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Screen sharing", NotificationManager.IMPORTANCE_LOW))
        val stop = PendingIntent.getService(
            this, 0, Intent(this, CaptureService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, CHANNEL)
            .setContentTitle("Remote Display is sharing this screen")
            .setSmallIcon(R.drawable.ic_stat_remote)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .build()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(1, notification)
        }

        val code = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        @Suppress("DEPRECATION")
        val data = intent.getParcelableExtra<Intent>(EXTRA_RESULT_DATA)
        if (data == null) {
            shutdown()
            return START_NOT_STICKY
        }
        val projection = getSystemService(MediaProjectionManager::class.java).getMediaProjection(code, data)
        try {
            streamer = Streamer(
                this, projection, intent.getIntExtra(EXTRA_MAX_SIZE, 1920), intent.getIntExtra(EXTRA_BITRATE, 8_000_000),
            ).also { it.start() }
            running = true
        } catch (e: Exception) {
            android.util.Log.e("CaptureService", "failed to start", e)
            shutdown()
        }
        return START_NOT_STICKY
    }

    private fun shutdown() {
        streamer?.stop()
        streamer = null
        running = false
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        streamer?.stop()
        streamer = null
        running = false
        super.onDestroy()
    }
}
