package com.kitty.app

import android.Manifest
import android.app.Activity
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class SecretArgs {
    lateinit var account: String
    var value: String? = null
}

@InvokeArg
class DownloadNoticeArgs {
    var title: String? = null
    var received: Long = 0
    var total: Long = 0
}

/**
 * The Android-native surface Kitty's Rust core cannot reach on its own:
 * hardware-backed secret storage and the download foreground service.
 *
 * Registered from Rust as a Tauri Android plugin (`crate::android`), which is
 * why this lives in the app module rather than in a separate Gradle library —
 * it is one app's glue, not a reusable plugin, and `find_class` resolves it
 * through the activity's own classloader either way.
 *
 * Every command is reachable only from Rust. `capabilities/default.json`
 * grants the webview nothing here, so no JS — including anything a model
 * might talk a tool into emitting — can read a stored API key.
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications")
    ]
)
class KittyPlugin(private val activity: Activity) : Plugin(activity) {

    // --- Secrets ---------------------------------------------------------

    @Command
    fun setSecret(invoke: Invoke) {
        val args = invoke.parseArgs(SecretArgs::class.java)
        val value = args.value
        if (value == null) {
            invoke.reject("setSecret requires a value")
            return
        }
        try {
            SecretStore.set(activity, args.account, value)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("could not store the secret: ${e.message}", e)
        }
    }

    /** Resolves `{ found: false }` for "nothing stored", rejects for "stored
     *  but unreadable". Collapsing those two into one answer is what the Rust
     *  side's `classify_read_result` exists to prevent. */
    @Command
    fun getSecret(invoke: Invoke) {
        val args = invoke.parseArgs(SecretArgs::class.java)
        try {
            val secret = SecretStore.get(activity, args.account)
            val result = JSObject()
            if (secret == null) {
                result.put("found", false)
            } else {
                result.put("found", true)
                result.put("value", secret)
            }
            invoke.resolve(result)
        } catch (e: Exception) {
            invoke.reject("could not read the secret: ${e.message}", e)
        }
    }

    @Command
    fun deleteSecret(invoke: Invoke) {
        val args = invoke.parseArgs(SecretArgs::class.java)
        try {
            SecretStore.delete(activity, args.account)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("could not delete the secret: ${e.message}", e)
        }
    }

    // --- Download foreground service -------------------------------------

    @Command
    fun startDownloadNotice(invoke: Invoke) {
        val args = invoke.parseArgs(DownloadNoticeArgs::class.java)
        try {
            DownloadService.start(
                activity,
                args.title ?: "Downloading model",
                args.received,
                args.total
            )
            invoke.resolve()
        } catch (e: Exception) {
            // A failed foreground-service start must not fail the download —
            // it only means the transfer is now at the mercy of Doze.
            invoke.reject("could not start the download service: ${e.message}", e)
        }
    }

    @Command
    fun stopDownloadNotice(invoke: Invoke) {
        try {
            DownloadService.stop(activity)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("could not stop the download service: ${e.message}", e)
        }
    }

    /** Ask for POST_NOTIFICATIONS (required from Android 13 for the download
     *  notification to be visible). Resolves `{ granted }` either way —
     *  a refusal is a normal answer, not an error: the service still runs, the
     *  user just does not see its progress. */
    @Command
    fun requestNotificationPermission(invoke: Invoke) {
        if (getPermissionState("notifications") == app.tauri.PermissionState.GRANTED) {
            invoke.resolve(JSObject().put("granted", true))
            return
        }
        requestPermissionForAlias("notifications", invoke, "notificationPermissionResult")
    }

    @PermissionCallback
    fun notificationPermissionResult(invoke: Invoke) {
        val granted = getPermissionState("notifications") == app.tauri.PermissionState.GRANTED
        invoke.resolve(JSObject().put("granted", granted))
    }
}
