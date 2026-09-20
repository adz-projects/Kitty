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
 * Keeps an agent turn alive while Kitty is in the background.
 *
 * On Android the whole stack — the in-process BigTiny daemon future, the
 * loopback HTTP hop, and every in-process MCP server — runs inside this one app
 * process (`src-tauri/src/lifecycle/bigtiny_embedded.rs`). With no foreground
 * component, Android freezes (and can kill) that process minutes after the user
 * switches to another app, stalling the turn. This service does no work itself;
 * its whole job is to hold the process in a state Android will not freeze for
 * the duration of a turn:
 *
 *  - A **foreground service** with a visible ongoing notification.
 *  - `dataSync` type — the category already declared for the download service
 *    and the one Play expects for background network/compute work. An agent
 *    turn runs minutes, well under the Android-15 ~6h/day `dataSync` budget,
 *    but the `onTimeout` guard is kept anyway so a stuck turn can't hang the
 *    service past the cap.
 *  - A bounded partial **wake lock**, so Deep Doze doesn't suspend the CPU
 *    mid-turn. One hour is far past any realistic turn and short enough that a
 *    missed `stop` (a panic, a kill) expires instead of draining the battery.
 *
 * Bracketed from Rust: started when the turn's SSE stream begins and stopped
 * the moment it ends (see `bigtiny::stream`'s `foreground::TurnSession`), so the
 * notification appears only while a turn is actually running. Modelled on
 * `DownloadService`; the difference is lifetime scope.
 */
class TurnService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    companion object {
        const val CHANNEL_ID = "kitty_turns"
        const val NOTIFICATION_ID = 4202

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, TurnService::class.java))
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, TurnService::class.java))
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ensureChannel()
        val notification = buildNotification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "kitty:agent-turn").apply {
                setReferenceCounted(false)
                // Bounded rather than indefinite: if the Rust side ever fails to
                // call stop (a panic, a kill), the lock expires instead of
                // draining the battery. One hour is well past any realistic turn.
                acquire(60 * 60 * 1000L)
            }
        }

        // NOT_STICKY: if Android kills us under memory pressure the turn died
        // with the process; resurrecting a bare service that represents no
        // running turn is pointless. The next turn starts a fresh service.
        return START_NOT_STICKY
    }

    /**
     * Android 15+ caps a `dataSync` foreground service at roughly six hours per
     * day and calls this when the budget runs out. Not stopping promptly is an
     * ANR, so we stop. A turn is minutes, so this should never fire in practice;
     * it exists so a wedged turn can't hang the service past the cap.
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
            "Working in the background",
            // LOW: a status readout the user can glance at, not something to
            // interrupt them with. It still can't be swiped away while the
            // service is foreground.
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "Shown while Kitty is finishing a turn in the background."
            setShowBadge(false)
        }
        manager.createNotificationChannel(channel)
    }

    private fun buildNotification(): Notification {
        val tapToOpen = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java)
                .setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_IMMUTABLE
        )

        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Kitty is working…")
            .setContentText("Finishing your turn in the background.")
            .setSmallIcon(R.drawable.ic_stat_activity)
            .setContentIntent(tapToOpen)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()
    }
}
