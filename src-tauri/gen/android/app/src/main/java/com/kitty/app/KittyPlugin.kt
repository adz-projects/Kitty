package com.kitty.app

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Base64
import android.webkit.MimeTypeMap
import android.webkit.WebView
import java.io.File
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class SecretArgs {
    lateinit var account: String
    var value: String? = null
}

@InvokeArg
class CopyContentUriArgs {
    lateinit var uri: String
    lateinit var destDir: String
}

@InvokeArg
class WriteDocumentArgs {
    /** Either a document URI (ACTION_CREATE_DOCUMENT) or a tree URI (ACTION_OPEN_DOCUMENT_TREE). */
    lateinit var uri: String
    /** Required when `uri` is a tree: the name of the document to create in it. */
    var fileName: String? = null
    var mimeType: String? = null
    /** Bytes to write, base64. Exactly one of this and `sourcePath` is set. */
    var contentBase64: String? = null
    /** A filesystem path to stream from instead, for payloads too big to base64. */
    var sourcePath: String? = null
}

@InvokeArg
class DownloadNoticeArgs {
    var title: String? = null
    var received: Long = 0
    var total: Long = 0
}

@InvokeArg
class NotifyArgs {
    var title: String? = null
    var body: String? = null
    /** `approval`, `finished`, `failed` or `degraded`: which channel (#75). */
    var channel: String? = null
    /** The chat it is about; tapping opens it. */
    var sessionId: String? = null
}

/**
 * The Android-native surface Kitty's Rust core cannot reach on its own:
 * hardware-backed secret storage, the download and agent-turn foreground
 * services, and posting system notifications (the Tauri notification plugin is
 * disabled on Android — its onNewIntent force-closes the app under singleTask).
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

    // --- Attachments -----------------------------------------------------

    /**
     * Copy a `content://` attachment into a real directory, and report the
     * name the user actually knows it by.
     *
     * The document picker (`ACTION_OPEN_DOCUMENT`, behind Tauri's dialog
     * plugin) hands back a `content://` URI, not a path. Rust cannot
     * `File::open` one — there is no file there — so an attached document
     * reached the model as an opaque URI it had no tool for. Worse, deriving a
     * display name from the URI's last segment produces the provider's
     * internal id (`msf%3A1000000123`), which is what the user saw on the
     * attachment chip.
     *
     * Both answers live behind the ContentResolver, so both are taken here in
     * one pass: `OpenableColumns.DISPLAY_NAME` for the name, and
     * `openInputStream` for the bytes.
     *
     * Resolves `{ name, path }` — the copy's final basename (which may be
     * de-duplicated) and its absolute path.
     */
    @Command
    fun copyContentUri(invoke: Invoke) {
        val args = invoke.parseArgs(CopyContentUriArgs::class.java)
        try {
            val dest = copyUriInto(Uri.parse(args.uri), File(args.destDir)) ?: run {
                invoke.reject("could not open the attachment: ${args.uri}")
                return
            }
            invoke.resolve(
                JSObject()
                    .put("name", dest.name)
                    .put("path", dest.absolutePath)
                    .put("bytes", dest.length())
            )
        } catch (e: Exception) {
            invoke.reject("could not copy the attachment: ${e.message}", e)
        }
    }

    /** The copy itself, shared with the share intake below. Null when the
     *  provider would not open a stream. */
    private fun copyUriInto(uri: Uri, dir: File): File? {
        val resolver = activity.contentResolver

        // The provider is the only thing that knows the human name. A
        // provider is allowed to answer nothing, so fall back to a
        // generic name with an extension derived from the MIME type —
        // the readers dispatch on extension, so losing that would break
        // the handoff even when the copy itself succeeded.
        var displayName: String? = null
        try {
            resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { c ->
                    if (c.moveToFirst()) {
                        val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                        if (i >= 0 && !c.isNull(i)) displayName = c.getString(i)
                    }
                }
        } catch (_: Exception) {
            // A provider that refuses the query is not a failed copy.
        }

        var name = displayName?.trim().orEmpty()
        if (name.isEmpty()) {
            val ext = MimeTypeMap.getSingleton()
                .getExtensionFromMimeType(resolver.getType(uri))
            name = if (ext.isNullOrEmpty()) "attachment" else "attachment.$ext"
        }
        // Strip anything that could escape the destination directory or
        // name a device on the host: the provider controls this string.
        name = name.map {
            if (it.isISOControl() || it in "\\/:*?\"<>|") "_" else it
        }.joinToString("")
        if (name == "." || name == "..") name = "attachment"

        dir.mkdirs()

        // Same de-duplication rule as the desktop copy path
        // (`commands::file::copy_file_into_chat_folder_blocking`): never
        // overwrite, and keep the readable name.
        val stem = name.substringBeforeLast('.', name)
        val ext = name.substringAfterLast('.', "")
        var dest = File(dir, name)
        var n = 2
        while (dest.exists()) {
            dest = File(dir, if (ext.isEmpty()) "$stem ($n)" else "$stem ($n).$ext")
            n++
        }

        resolver.openInputStream(uri)?.use { input ->
            dest.outputStream().use { output -> input.copyTo(output) }
        } ?: return null
        return dest
    }

    // --- Incoming intents (A2) --------------------------------------------

    /**
     * What Android handed the app from outside — a share (A3) or a tapped
     * notification (A4) — queued for Rust to collect with
     * [takePendingIntents]. Rust asks rather than being told because the
     * webview may not be listening yet: a share can be what launched the app.
     *
     * Deliberately in this plugin rather than Tauri's notification plugin,
     * whose own `onNewIntent` crashes the app (see `lib.rs`).
     */
    private val pendingIntents = mutableListOf<JSObject>()
    /** Shares whose files are still being copied, so a poll can wait. */
    private var copying = 0

    override fun load(webView: WebView) {
        super.load(webView)
        // A cold start: the intent that launched the activity.
        capture(activity.intent)
    }

    override fun onNewIntent(intent: Intent) {
        capture(intent)
    }

    private fun capture(intent: Intent?) {
        if (intent == null || intent.getBooleanExtra(EXTRA_CONSUMED, false)) return
        // The activity keeps its launch intent: never read the same one twice.
        intent.putExtra(EXTRA_CONSUMED, true)
        when (intent.action) {
            Intent.ACTION_SEND, Intent.ACTION_SEND_MULTIPLE -> captureShare(intent)
            else -> {
                val sessionId = intent.getStringExtra(EXTRA_SESSION_ID) ?: return
                val item = JSObject().put("kind", "open_chat").put("sessionId", sessionId)
                synchronized(pendingIntents) { pendingIntents.add(item) }
            }
        }
    }

    /** Copy a share's files out of the sending app now, while the read grant
     *  that came with the intent still holds — off the main thread, since a
     *  shared video must not freeze the UI. */
    private fun captureShare(intent: Intent) {
        val uris = sharedUris(intent)
        val text = intent.getStringExtra(Intent.EXTRA_TEXT)
        val subject = intent.getStringExtra(Intent.EXTRA_SUBJECT)
        synchronized(pendingIntents) { copying++ }
        Thread {
            val paths = JSArray()
            val failed = JSArray()
            val dir = File(activity.cacheDir, "shared/${System.currentTimeMillis()}")
            for (uri in uris) {
                try {
                    val copy = copyUriInto(uri, dir)
                    if (copy != null) paths.put(copy.absolutePath) else failed.put(uri.toString())
                } catch (_: Exception) {
                    failed.put(uri.toString())
                }
            }
            val item = JSObject()
                .put("kind", "share")
                .put("text", text ?: "")
                .put("subject", subject ?: "")
                .put("paths", paths)
                .put("failed", failed.length())
            synchronized(pendingIntents) {
                pendingIntents.add(item)
                copying--
            }
        }.start()
    }

    private fun sharedUris(intent: Intent): List<Uri> {
        val out = mutableListOf<Uri>()
        if (intent.action == Intent.ACTION_SEND_MULTIPLE) {
            val list = if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM)
            }
            list?.let { out.addAll(it) }
        } else {
            val one = if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM)
            }
            one?.let { out.add(it) }
        }
        return out
    }

    /** Resolves `{ intents: [...], copying: Boolean }` and empties the
     *  queue. `copying` says a share is still arriving: ask again shortly. */
    @Command
    fun takePendingIntents(invoke: Invoke) {
        val list = JSArray()
        val busy: Boolean
        synchronized(pendingIntents) {
            pendingIntents.forEach { list.put(it) }
            pendingIntents.clear()
            busy = copying > 0
        }
        invoke.resolve(JSObject().put("intents", list).put("copying", busy))
    }

    // --- Saving out through the Storage Access Framework ------------------

    /**
     * Write a document the user picked, and report how many bytes landed.
     *
     * This is the only way anything leaves Kitty on Android. The chat folder
     * is inside the app's private data directory, so a file the model wrote is
     * invisible to every other app until it is copied out through a URI the
     * user granted. Rust cannot do that copy: `content://` is not a path, and
     * `java.io.File`/`std::fs` cannot open one.
     *
     * Takes both URI kinds because the two save flows produce different ones.
     * ACTION_CREATE_DOCUMENT (the save dialog) returns a *document* URI that
     * already exists and is written directly. ACTION_OPEN_DOCUMENT_TREE (the
     * folder picker, used by the bulk chat export) returns a *tree* URI, which
     * names a directory and cannot be written at all — a document has to be
     * created inside it first, via DocumentsContract. Appending "/name.jsonl"
     * to a tree URI, which is what a filesystem-shaped API invites, produces a
     * string that is not a valid URI of either kind.
     *
     * `sourcePath` streams instead of taking base64, because an artifact can
     * be a 40 MB PDF and base64 through the JSON bridge would mean holding it
     * three times over.
     *
     * The byte count is returned rather than assumed. A provider can accept a
     * write and commit nothing — Google Drive uploads asynchronously after the
     * stream closes — so the caller verifies rather than trusting `Ok`.
     */
    @Command
    fun writeDocument(invoke: Invoke) {
        val args = invoke.parseArgs(WriteDocumentArgs::class.java)
        try {
            val resolver = activity.contentResolver
            val picked = Uri.parse(args.uri)

            // A tree URI names a directory: create the document first. The
            // `isTreeUri` check is what tells the two apart -- a caller cannot
            // be trusted to know which dialog produced the URI it was handed.
            val target = if (DocumentsContract.isTreeUri(picked)) {
                val name = args.fileName
                    ?: run {
                        invoke.reject("a folder was chosen but no file name was given")
                        return
                    }
                val parent = DocumentsContract.buildDocumentUriUsingTree(
                    picked,
                    DocumentsContract.getTreeDocumentId(picked)
                )
                // The provider owns the final name: it de-duplicates against
                // what is already there (`report.jsonl` -> `report (1).jsonl`)
                // and may adjust the extension to match the MIME type. Which
                // is why nothing here assumes the name it asked for.
                DocumentsContract.createDocument(
                    resolver,
                    parent,
                    args.mimeType ?: "application/octet-stream",
                    name
                ) ?: run {
                    invoke.reject("could not create \"$name\" in the chosen folder")
                    return
                }
            } else {
                picked
            }

            // Mode "w", not "wt". Truncation is meaningless on a document that
            // was just created, and several providers -- Drive among them --
            // reject or silently mishandle the "t" flag.
            val written = resolver.openOutputStream(target, "w")?.use { output ->
                val src = args.sourcePath
                if (src != null) {
                    File(src).inputStream().use { input -> input.copyTo(output) }
                } else {
                    val bytes = Base64.decode(args.contentBase64 ?: "", Base64.DEFAULT)
                    output.write(bytes)
                    // `copyTo` reports its own count; this branch has to.
                    bytes.size.toLong()
                }.also { output.flush() }
            } ?: run {
                invoke.reject("the chosen location could not be opened for writing")
                return
            }

            invoke.resolve(
                JSObject()
                    .put("uri", target.toString())
                    .put("bytes", written)
            )
        } catch (e: Exception) {
            invoke.reject("could not save the file: ${e.message}", e)
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

    // --- Turn foreground service ----------------------------------------

    /** Start (or no-op if already running) the agent-turn foreground service,
     *  which holds the process alive so an in-progress turn keeps running while
     *  Kitty is backgrounded. Bracketed from Rust around the turn's SSE stream
     *  (`bigtiny::stream`). */
    @Command
    fun startTurnNotice(invoke: Invoke) {
        try {
            TurnService.start(activity)
            invoke.resolve()
        } catch (e: Exception) {
            // A failed foreground-service start must not fail the turn — it only
            // means the turn is now at the mercy of Doze if backgrounded.
            invoke.reject("could not start the turn service: ${e.message}", e)
        }
    }

    @Command
    fun stopTurnNotice(invoke: Invoke) {
        try {
            TurnService.stop(activity)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("could not stop the turn service: ${e.message}", e)
        }
    }

    // --- One-shot notifications -----------------------------------------

    /** Post a dismissable system notification (tap opens the app). This is the
     *  Android backend for `notifications::emit_notification` — the Tauri
     *  notification plugin is disabled here (its onNewIntent force-closes the
     *  app under singleTask), so Rust posts through this instead. Best-effort:
     *  a missing permission or a failed post is a degradation, not an error. */
    @Command
    fun postNotification(invoke: Invoke) {
        val args = invoke.parseArgs(NotifyArgs::class.java)
        try {
            val channel = ensureChannels(args.channel)
            val id = nextAlertId()
            // Tapping opens the chat it is about (#75), through [capture]. A
            // distinct request code per notification, or Android would reuse
            // one PendingIntent — and one chat — for all of them.
            val open = Intent(activity, MainActivity::class.java)
                .setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            args.sessionId?.let { open.putExtra(EXTRA_SESSION_ID, it) }
            val tapToOpen = PendingIntent.getActivity(
                activity,
                id,
                open,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            val notification = Notification.Builder(activity, channel)
                .setContentTitle(args.title ?: "Kitty")
                .setContentText(args.body ?: "")
                .setSmallIcon(R.drawable.ic_stat_activity)
                .setContentIntent(tapToOpen)
                .setAutoCancel(true)
                .build()
            val manager =
                activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            // A fresh id each post so a new toast doesn't silently replace an
            // unread one; the low ceiling keeps ids from growing unbounded.
            manager.notify(id, notification)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("could not post a notification: ${e.message}", e)
        }
    }

    /**
     * One channel per kind of notice (#75), so the system Settings app — which
     * owns these toggles on Android — can silence finished replies while
     * keeping approvals loud. Returns the channel id for `kind`.
     */
    private fun ensureChannels(kind: String?): String {
        val manager =
            activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (manager.getNotificationChannel(CHANNEL_APPROVAL) == null) {
            fun make(id: String, name: String, importance: Int, what: String) =
                NotificationChannel(id, name, importance).apply { description = what }
            manager.createNotificationChannels(
                listOf(
                    // HIGH: a turn is paused until someone answers.
                    make(
                        CHANNEL_APPROVAL, "Approvals needed",
                        NotificationManager.IMPORTANCE_HIGH,
                        "Kitty is waiting for you to allow or deny a tool."
                    ),
                    make(
                        CHANNEL_FINISHED, "Replies finished",
                        NotificationManager.IMPORTANCE_DEFAULT,
                        "A reply finished while you were elsewhere."
                    ),
                    make(
                        CHANNEL_FAILED, "Problems",
                        NotificationManager.IMPORTANCE_DEFAULT,
                        "A reply or a scheduled task failed."
                    ),
                    make(
                        CHANNEL_DEGRADED, "Engine status",
                        NotificationManager.IMPORTANCE_LOW,
                        "Kitty's engine stopped or came back."
                    ),
                )
            )
            // The single channel everything used to share.
            manager.deleteNotificationChannel(LEGACY_CHANNEL_ID)
        }
        return when (kind) {
            "approval" -> CHANNEL_APPROVAL
            "failed" -> CHANNEL_FAILED
            "degraded" -> CHANNEL_DEGRADED
            else -> CHANNEL_FINISHED
        }
    }

    private fun nextAlertId(): Int {
        // 4300..4399 — disjoint from the foreground services' fixed ids
        // (4201 downloads, 4202 turns).
        alertSeq = (alertSeq + 1) % 100
        return 4300 + alertSeq
    }

    companion object {
        private const val LEGACY_CHANNEL_ID = "kitty_alerts"
        private const val CHANNEL_APPROVAL = "kitty_approval"
        private const val CHANNEL_FINISHED = "kitty_finished"
        private const val CHANNEL_FAILED = "kitty_failed"
        private const val CHANNEL_DEGRADED = "kitty_degraded"
        private const val EXTRA_SESSION_ID = "com.kitty.app.SESSION_ID"
        private const val EXTRA_CONSUMED = "com.kitty.app.CONSUMED"
        private var alertSeq = 0
    }
}
