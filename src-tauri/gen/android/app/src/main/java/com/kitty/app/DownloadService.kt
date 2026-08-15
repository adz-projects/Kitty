package com.kitty.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager

/**
 * Keeps a model download alive while Kitty is in the background.
 *
 * The download itself runs in Rust (`src-tauri/src/commands/models.rs`), not
 * here — this service does not move a single byte. Its whole job is to hold
 * the process in a state Android will not freeze:
 *
 *  - A **foreground service** with a visible notification. Without one, a
 *    backgrounded app loses its network access within minutes and is a
 *    candidate for outright death; a multi-GB GGUF over cellular takes far
 *    longer than that.
 *  - `dataSync` type, which is the category Android defines for exactly this
 *    and the one Play expects to see declared for it.
 *  - A partial **wake lock**, because Deep Doze otherwise suspends the CPU
 *    between maintenance windows and stalls the transfer for minutes at a
 *    time. Released in `onDestroy`, and it is a plain partial lock — the
 *    screen is free to turn off.
 *
 * Network handoff (Wi-Fi to cellular and back) is deliberately *not* handled
 * here with a `ConnectivityManager.NetworkCallback`. The socket dies on
 * handoff whatever we observe, and the Rust downloader already resumes from
 * the `.part` file's byte offset with a `Range` header — so a bounded retry
 * loop on the Rust side recovers from a handoff, a tunnel, and a flaky AP
 * with one mechanism instead of three.
 */
class DownloadService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    companion object {
        const val CHANNEL_ID = "kitty_downloads"
        const val NOTIFICATION_ID = 4201
        const val EXTRA_TITLE = "title"
        const val EXTRA_RECEIVED = "received"
        const val EXTRA_TOTAL = "total"

        /** Start, or update the notification of an already-running instance —
         *  `startForegroundService` on a started service just delivers another
         *  `onStartCommand`, which is how progress updates arrive. */
        fun start(ctx: Context, title: String, received: Long, total: Long) {
            val intent = Intent(ctx, DownloadService::class.java)
                .putExtra(EXTRA_TITLE, title)
                .putExtra(EXTRA_RECEIVED, received)
                .putExtra(EXTRA_TOTAL, total)
            ctx.startForegroundService(intent)
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, DownloadService::class.java))
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val title = intent?.getStringExtra(EXTRA_TITLE) ?: "Downloading model"
        val received = intent?.getLongExtra(EXTRA_RECEIVED, 0L) ?: 0L
        val total = intent?.getLongExtra(EXTRA_TOTAL, 0L) ?: 0L

        ensureChannel()
        val notification = buildNotification(title, received, total)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "kitty:model-download").apply {
                setReferenceCounted(false)
                // Bounded rather than indefinite: if the Rust side ever fails
                // to call stop (a panic, a kill), the lock expires instead of
                // draining the battery until the next reboot. Six hours is
                // well past any realistic download and well short of "nobody
                // noticed for a day".
                acquire(6 * 60 * 60 * 1000L)
            }
        }

        // STICKY, not REDELIVER: if Android kills us under memory pressure the
        // service coming back is pointless on its own — the Rust download died
        // with the process. Restarting bare lets `onStartCommand` run with a
        // null intent, post a generic notification, and be stopped by the next
        // launch, rather than resurrecting a stale progress figure.
        return START_STICKY
    }

    /**
     * Android 15+ caps a `dataSync` foreground service at roughly six hours
     * per day and calls this when the budget runs out. Not stopping promptly
     * is an ANR, so we stop — a download still running at that point loses its
     * foreground status and will be throttled, which is the platform's answer
     * and not something an app can argue with. The six-hour wake-lock cap in
     * `onStartCommand` is set to the same horizon deliberately.
     */
    override fun onTimeout(startId: Int, fgsType: Int) {
        stopSelf()
    }

    override fun onDestroy() {
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        super.onDestroy()
    }

    private fun ensureChannel() {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Model downloads",
            // LOW: this notification is a progress readout the user can glance
            // at, not something to interrupt them with. It still cannot be
            // swiped away while the service is foreground.
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "Shown while Kitty is downloading a model in the background."
            setShowBadge(false)
        }
        manager.createNotificationChannel(channel)
    }

    private fun buildNotification(title: String, received: Long, total: Long): Notification {
        val tapToOpen = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java)
                .setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_IMMUTABLE
        )

        val builder = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(title)
            .setSmallIcon(R.drawable.ic_stat_download)
            .setContentIntent(tapToOpen)
            .setOngoing(true)
            .setOnlyAlertOnce(true)

        if (total > 0) {
            val percent = ((received.toDouble() / total.toDouble()) * 100).toInt().coerceIn(0, 100)
            builder.setContentText("$percent%  ·  ${humanBytes(received)} of ${humanBytes(total)}")
            builder.setProgress(100, percent, false)
        } else {
            // Unknown length (no Content-Length on the response) — an
            // indeterminate bar is honest; a fake percentage is not.
            builder.setContentText(humanBytes(received))
            builder.setProgress(0, 0, true)
        }
        return builder.build()
    }

    private fun humanBytes(n: Long): String {
        if (n < 1024) return "$n B"
        val units = arrayOf("KB", "MB", "GB", "TB")
        var value = n.toDouble() / 1024.0
        var unit = 0
        while (value >= 1024.0 && unit < units.size - 1) {
            value /= 1024.0
            unit++
        }
        return String.format("%.1f %s", value, units[unit])
    }
}
