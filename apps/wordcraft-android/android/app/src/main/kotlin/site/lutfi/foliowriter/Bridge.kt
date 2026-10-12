package site.lutfi.foliowriter

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.database.Cursor
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import androidx.annotation.Keep
import androidx.core.content.FileProvider
import com.google.androidgamesdk.gametextinput.State
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.lang.ref.WeakReference
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

private fun JSONObject.optLongOrNull(key: String): Long? = if (has(key) && !isNull(key)) optLong(key) else null

/**
 * What the native host (`src/platform.rs`, `src/android/bridge.rs`) asks of Android, and what Android
 * reports back. The host calls [call] with an operation and JSON arguments on its own thread; the answer,
 * if any, is JSON. Reports go to [nativeEvent] as JSON, from the UI thread or the I/O thread, never from
 * inside [call].
 *
 * Documents are copied between their `content://` URIs and the host's copies (its "mirrors") as streams on
 * one I/O thread, so the host never waits on storage.
 *
 * Kept whole by R8 (`@Keep`): only native code calls these methods.
 */
@Keep
object Bridge {
    private const val TAG = "FolioWriter"
    private const val LABEL = "Folio Writer"
    /** Documents larger than this aren't copied in (the app refuses files over 2 GB anyway). */
    private const val MAX_COPY = 1L shl 30

    @Volatile private var current: WeakReference<MainActivity>? = null
    @Volatile private var context: Context? = null
    private val io: ExecutorService = Executors.newSingleThreadExecutor { r -> Thread(r, "folio-io").apply { isDaemon = true } }

    /** From [MainActivity.onCreate], before the native thread starts. */
    fun attach(activity: MainActivity) {
        current = WeakReference(activity)
        context = activity.applicationContext
        nativeInit()
    }

    fun detach(activity: MainActivity) {
        if (current?.get() === activity) current = null
    }

    private fun live(): MainActivity? = current?.get()?.takeUnless { it.isFinishing || it.isDestroyed }

    /** Runs [block] on the UI thread with the live activity; false when there's none. */
    private fun onUi(block: (MainActivity) -> Unit): Boolean {
        val activity = live() ?: return false
        activity.runOnUiThread {
            val now = live() ?: return@runOnUiThread
            try {
                block(now)
            } catch (e: RuntimeException) {
                Log.w(TAG, "bridge: ${e.javaClass.name}")
            }
        }
        return true
    }

    private fun resolver(): ContentResolver? = context?.contentResolver

    /** The native host's calls. Returns at once; never throws. */
    fun call(op: String, args: String): String? = try {
        val a = JSONObject(args)
        when (op) {
            "pick" -> pick(a)
            "copyIn" -> copyIn(a)
            "writeBack" -> writeBack(a)
            "refresh" -> refresh(a)
            "grants" -> grants()
            "release" -> release(a)
            "clipboardGet" -> clipboardGet()
            "clipboardSet" -> clipboardSet(a)
            "openUrl" -> ok(openUrl(a.optString("url")))
            "pointerIcon" -> { onUi { it.setPointer(a.optInt("type", 1000)) }; null }
            "print" -> ok(onUi { it.printPdf(File(a.getString("path")), a.optString("name", LABEL)) })
            "share" -> ok(share(a))
            "moveTaskToBack" -> ok(onUi { it.moveTaskToBack(true) })
            "imePush" -> {
                val text = a.getString("text")
                val anchor = a.getInt("anchor")
                val focus = a.getInt("focus")
                val generation = a.getLong("generation")
                onUi { it.imePush(text, anchor, focus, generation) }
                null
            }
            else -> null
        }
    } catch (e: Exception) {
        Log.w(TAG, "$op: ${e.javaClass.name}")
        null
    }

    private fun ok(ok: Boolean) = JSONObject().put("ok", ok).toString()

    private fun report(event: JSONObject) = nativeEvent(event.toString())

    /** The keyboard's buffer, for the host (`MainActivity.stateChanged`, UI thread). */
    internal fun imeState(state: State, generation: Long) {
        report(
            JSONObject()
                .put("event", "ime")
                .put("text", state.text ?: "")
                .put("selStart", state.selectionStart)
                .put("selEnd", state.selectionEnd)
                .put("compStart", state.composingRegionStart)
                .put("compEnd", state.composingRegionEnd)
                .put("generation", generation),
        )
    }

    // Pickers ------------------------------------------------------------------------------------

    private fun pick(a: JSONObject): String? {
        val req = a.getLong("req")
        val create = a.optJSONObject("create")
        val persist = a.optBoolean("persist")
        val mimes = a.optJSONArray("mimes")?.let { arr -> Array(arr.length()) { arr.getString(it) } } ?: emptyArray()
        val shown = onUi {
            if (create != null) {
                it.createDocument(req, create.optString("name", "Document1.docx"), create.optString("mime", "application/octet-stream"))
            } else {
                it.openDocument(req, mimes, persist)
            }
        }
        if (!shown) report(JSONObject().put("event", "picked").put("req", req).put("error", "no window"))
        return null
    }

    /** The picker closed ([MainActivity]'s launchers): describe the document, keeping its permission. */
    internal fun picked(req: Long, result: Intent?, create: Boolean, persist: Boolean) {
        val uri = result?.data
        if (uri == null) {
            report(JSONObject().put("event", "picked").put("req", req).put("cancelled", true))
            return
        }
        val flags = result.flags
        io.execute {
            val event = try {
                describe(uri, create, persist || create, flags).put("event", "picked").put("req", req)
            } catch (e: Exception) {
                JSONObject().put("event", "picked").put("req", req).put("error", e.javaClass.name)
            }
            report(event)
        }
    }

    /** No picker could be shown (no app handles it): the host shows its own file browser. */
    internal fun pickFailed(req: Long, e: Exception) {
        Log.w(TAG, "no document picker: ${e.javaClass.name}")
        report(JSONObject().put("event", "picked").put("req", req).put("error", e.javaClass.name))
    }

    /** Documents sent to the app (Open with, Share). */
    internal fun incoming(uris: List<Uri>, flags: Int) {
        if (uris.isEmpty()) return
        io.execute {
            val documents = JSONArray()
            for (uri in uris.take(16)) {
                try {
                    documents.put(describe(uri, false, true, flags))
                } catch (e: Exception) {
                    Log.w(TAG, "incoming: ${e.javaClass.name}")
                }
            }
            report(JSONObject().put("event", "incoming").put("documents", documents))
        }
    }

    /** A document's name, type, size, last change and whether the app may write it. */
    private fun describe(uri: Uri, create: Boolean, persist: Boolean, flags: Int): JSONObject {
        val resolver = resolver() ?: throw IllegalStateException()
        // Keep the permission when the picker offered it (Open with's never is): read and write, else read.
        var kept = 0
        if (persist && uri.scheme == ContentResolver.SCHEME_CONTENT && flags and Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION != 0) {
            val offered = flags and (Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            for (mode in intArrayOf(offered, Intent.FLAG_GRANT_READ_URI_PERMISSION)) {
                if (mode == 0 || mode and offered != mode) continue
                try {
                    resolver.takePersistableUriPermission(uri, mode)
                    kept = mode
                    break
                } catch (e: SecurityException) {
                    // Not offered in this mode.
                }
            }
        }
        val meta = query(resolver, uri)
        val docFlags = meta.flags
        val supportsWrite = docFlags?.let { it and DocumentsContract.Document.FLAG_SUPPORTS_WRITE != 0 }
        val granted = flags and Intent.FLAG_GRANT_WRITE_URI_PERMISSION != 0 && (kept == 0 || kept and Intent.FLAG_GRANT_WRITE_URI_PERMISSION != 0)
        val writable = create || (granted && supportsWrite != false)
        val persisted = kept != 0
        return JSONObject()
            .put("uri", uri.toString())
            .put("name", meta.name ?: uri.lastPathSegment ?: "Document")
            .put("mime", meta.mime ?: resolver.getType(uri) ?: JSONObject.NULL)
            .put("writable", writable)
            .put("persisted", persisted)
            .put("virtual", docFlags?.let { it and DocumentsContract.Document.FLAG_VIRTUAL_DOCUMENT != 0 } ?: false)
            .put("remote", meta.remote())
    }

    private class Meta(val name: String?, val mime: String?, val size: Long?, val modified: Long?, val flags: Int?) {
        fun remote(): JSONObject = JSONObject().put("modified", modified ?: JSONObject.NULL).put("size", size ?: JSONObject.NULL)

        /** Whether the document seems changed since [was]: both known values differ, or the only known one does. */
        fun changedSince(was: JSONObject?): Boolean {
            val m0 = was?.optLongOrNull("modified")
            val s0 = was?.optLongOrNull("size")
            val m = if (m0 != null && modified != null) m0 != modified else null
            val s = if (s0 != null && size != null) s0 != size else null
            return when {
                m != null && s != null -> m && s
                else -> m ?: s ?: false
            }
        }
    }

    private fun query(resolver: ContentResolver, uri: Uri): Meta {
        if (uri.scheme == ContentResolver.SCHEME_FILE) {
            val f = File(uri.path ?: "")
            return Meta(f.name, null, f.length(), f.lastModified(), null)
        }
        return try {
            resolver.query(uri, null, null, null, null)?.use { c ->
                if (!c.moveToFirst()) return Meta(null, null, null, null, null)
                Meta(
                    c.string(OpenableColumns.DISPLAY_NAME),
                    c.string(DocumentsContract.Document.COLUMN_MIME_TYPE),
                    c.long(OpenableColumns.SIZE),
                    c.long(DocumentsContract.Document.COLUMN_LAST_MODIFIED),
                    c.long(DocumentsContract.Document.COLUMN_FLAGS)?.toInt(),
                )
            } ?: Meta(null, null, null, null, null)
        } catch (e: RuntimeException) {
            Meta(null, null, null, null, null)
        }
    }

    private fun Cursor.string(column: String): String? {
        val i = getColumnIndex(column)
        return if (i >= 0 && !isNull(i)) getString(i) else null
    }

    private fun Cursor.long(column: String): Long? {
        val i = getColumnIndex(column)
        return if (i >= 0 && !isNull(i)) getLong(i) else null
    }

    // Copies ------------------------------------------------------------------------------------

    private fun copyIn(a: JSONObject): String? {
        val req = a.getLong("req")
        val uri = Uri.parse(a.getString("uri"))
        val path = File(a.getString("path"))
        io.execute {
            val event = JSONObject().put("event", "copiedIn").put("req", req)
            try {
                val resolver = resolver() ?: throw IllegalStateException()
                val meta = query(resolver, uri)
                if ((meta.size ?: 0) > MAX_COPY) throw IOException("too large")
                val stream = resolver.openInputStream(uri) ?: throw FileNotFoundException()
                stream.use { copyAtomically(it, path) }
                event.put("remote", meta.remote())
            } catch (e: Exception) {
                event.put("error", e.javaClass.name)
            }
            report(event)
        }
        return null
    }

    /** Writes [from] to [to] through a file beside it, renamed over it once complete. */
    private fun copyAtomically(from: InputStream, to: File) {
        to.parentFile?.mkdirs()
        val temp = File(to.parentFile, ".${to.name}.copy")
        try {
            FileOutputStream(temp).use { out ->
                var total = 0L
                val buffer = ByteArray(64 * 1024)
                while (true) {
                    val n = from.read(buffer)
                    if (n < 0) break
                    total += n
                    if (total > MAX_COPY) throw IOException("too large")
                    out.write(buffer, 0, n)
                }
                out.fd.sync()
            }
            if (!temp.renameTo(to)) throw IOException("rename")
        } finally {
            temp.delete()
        }
    }

    private fun writeBack(a: JSONObject): String? {
        val entry = a.getLong("entry")
        val uri = Uri.parse(a.getString("uri"))
        val path = File(a.getString("path"))
        val expect = a.optJSONObject("expect")
        val force = a.optBoolean("force")
        io.execute {
            val event = JSONObject().put("event", "wroteBack").put("entry", entry)
            try {
                val resolver = resolver() ?: throw IllegalStateException()
                val before = query(resolver, uri)
                if (!force && before.changedSince(expect)) {
                    event.put("conflict", true).put("remote", before.remote())
                } else {
                    write(resolver, uri, path)
                    event.put("remote", query(resolver, uri).remote())
                }
            } catch (e: SecurityException) {
                event.put("error", e.javaClass.name).put("lost", true)
            } catch (e: FileNotFoundException) {
                event.put("error", e.javaClass.name).put("lost", true)
            } catch (e: Exception) {
                event.put("error", e.javaClass.name)
            }
            report(event)
        }
        return null
    }

    /**
     * Replaces the document's content with [path]'s. Providers differ on which modes they take and whether
     * "w" truncates: "wt", then "rwt", then "w", and the file is truncated to the new length where it can
     * be, so no old bytes are left after the end.
     */
    private fun write(resolver: ContentResolver, uri: Uri, path: File) {
        var pfd: android.os.ParcelFileDescriptor? = null
        var last: Exception? = null
        for (mode in arrayOf("wt", "rwt", "w")) {
            try {
                pfd = resolver.openFileDescriptor(uri, mode)
                if (pfd != null) break
            } catch (e: IllegalArgumentException) {
                last = e
            } catch (e: UnsupportedOperationException) {
                last = e
            }
        }
        val fd = pfd ?: throw (last as? RuntimeException ?: FileNotFoundException())
        android.os.ParcelFileDescriptor.AutoCloseOutputStream(fd).use { out ->
            FileInputStream(path).use { it.copyTo(out, 64 * 1024) }
            out.flush()
            try {
                out.channel.truncate(out.channel.position())
            } catch (e: IOException) {
                // A pipe: nothing to truncate.
            }
            try {
                out.fd.sync()
            } catch (e: IOException) {
                // Not a file: the provider syncs.
            }
        }
    }

    private fun refresh(a: JSONObject): String? {
        val entry = a.getLong("entry")
        val uri = Uri.parse(a.getString("uri"))
        val path = File(a.getString("path"))
        val expect = a.optJSONObject("expect")
        io.execute {
            val event = JSONObject().put("event", "refreshed").put("entry", entry)
            try {
                val resolver = resolver() ?: throw IllegalStateException()
                val now = query(resolver, uri)
                if (now.changedSince(expect)) {
                    val stream = resolver.openInputStream(uri) ?: throw FileNotFoundException()
                    stream.use { copyAtomically(it, path) }
                    event.put("changed", true).put("remote", now.remote())
                }
            } catch (e: Exception) {
                event.put("error", e.javaClass.name)
            }
            report(event)
        }
        return null
    }

    private fun grants(): String? {
        val resolver = resolver() ?: return null
        val uris = JSONArray()
        for (p in resolver.persistedUriPermissions) uris.put(p.uri.toString())
        return JSONObject().put("uris", uris).toString()
    }

    private fun release(a: JSONObject): String? {
        val resolver = resolver() ?: return null
        val uri = Uri.parse(a.getString("uri"))
        for (mode in intArrayOf(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION, Intent.FLAG_GRANT_READ_URI_PERMISSION)) {
            try {
                resolver.releasePersistableUriPermission(uri, mode)
                break
            } catch (e: SecurityException) {
                // Not held in this mode (or already gone).
            }
        }
        return null
    }

    // Clipboard, links, sharing ---------------------------------------------------------------------

    /** The clipboard's text (Android gives it only to the app with the focus). */
    private fun clipboardGet(): String? {
        val ctx = context ?: return null
        val clip = ctx.getSystemService(ClipboardManager::class.java)?.primaryClip ?: return null
        if (clip.itemCount < 1) return null
        val text = clip.getItemAt(0).coerceToText(ctx)?.toString() ?: return null
        return JSONObject().put("text", text).toString()
    }

    private fun clipboardSet(a: JSONObject): String? {
        val manager = context?.getSystemService(ClipboardManager::class.java) ?: return null
        manager.setPrimaryClip(ClipData.newPlainText(LABEL, a.optString("text")))
        return null
    }

    private fun openUrl(url: String): Boolean {
        val uri = Uri.parse(url)
        if (uri.scheme?.lowercase() !in setOf("http", "https", "mailto")) return false
        return onUi { activity ->
            try {
                activity.startActivity(Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE))
            } catch (e: ActivityNotFoundException) {
                Log.i(TAG, "no app opens links")
            }
        }
    }

    private fun share(a: JSONObject): Boolean {
        val file = File(a.getString("path"))
        val mime = a.optString("mime", "application/octet-stream")
        return onUi { activity ->
            val uri = FileProvider.getUriForFile(activity, "${activity.packageName}.files", file, a.optString("name", file.name))
            val send = Intent(Intent.ACTION_SEND).setType(mime).putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            send.clipData = ClipData.newRawUri(null, uri)
            activity.startActivity(Intent.createChooser(send, null))
        }
    }

    /** The activity is stopping: the host writes the settings and a snapshot of unsaved changes. */
    fun stopping() = nativeStopping()

    /**
     * Waits up to [ms] for what [stopping] started (the process could be killed once `onStop` returns). The
     * native thread does the writing; this only waits.
     */
    fun awaitSaved(ms: Int) = nativeAwaitSaved(ms)

    private external fun nativeInit()
    private external fun nativeEvent(json: String)
    private external fun nativeStopping()
    private external fun nativeAwaitSaved(ms: Int)
}
