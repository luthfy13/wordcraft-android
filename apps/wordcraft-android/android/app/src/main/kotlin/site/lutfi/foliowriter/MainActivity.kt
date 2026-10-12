package site.lutfi.foliowriter

import android.app.Activity
import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.print.PageRange
import android.print.PrintAttributes
import android.print.PrintDocumentAdapter
import android.print.PrintDocumentInfo
import android.print.PrintManager
import android.util.Log
import android.view.KeyEvent
import android.view.PointerIcon
import android.view.View
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContract
import androidx.core.content.IntentCompat
import androidx.core.view.WindowInsetsCompat
import com.google.androidgamesdk.GameActivity
import com.google.androidgamesdk.gametextinput.State
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream

/**
 * The window for the native app (`libwordcraft_android.so`), which draws and handles input itself.
 * This class only does what needs the Android SDK: the system pickers, documents sent to the app, printing
 * and the pointer's shape. [Bridge] carries the native host's calls here.
 */
class MainActivity : GameActivity() {
    private var pendingPick = NO_REQUEST
    private var pendingPersist = false
    private var pendingCreate = NO_REQUEST
    private var pointerType = PointerIcon.TYPE_ARROW
    /** The last text the native host pushed to the keyboard; its states carry it. UI thread only. */
    private var imeGeneration = 0L

    private val openLauncher = registerForActivityResult(OpenDocument()) { result ->
        val req = pendingPick
        pendingPick = NO_REQUEST
        if (req != NO_REQUEST) Bridge.picked(req, result, false, pendingPersist)
    }

    private val createLauncher = registerForActivityResult(CreateDocument()) { result ->
        val req = pendingCreate
        pendingCreate = NO_REQUEST
        if (req != NO_REQUEST) Bridge.picked(req, result, true, true)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // GameActivity starts the native thread in super.onCreate, which reads the launch flags at once.
        System.loadLibrary("wordcraft_android")
        nativeSetLaunchArgs(intent?.getStringExtra(EXTRA_ARGS).orEmpty())
        Bridge.attach(this)
        super.onCreate(savedInstanceState)
        savedInstanceState?.let {
            pendingPick = it.getLong(KEY_PICK, NO_REQUEST)
            pendingPersist = it.getBoolean(KEY_PERSIST, false)
            pendingCreate = it.getLong(KEY_CREATE, NO_REQUEST)
        }
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() = sendBackToApp()
        })
        // A document this activity was started for (not again when it's recreated).
        if (savedInstanceState == null) receive(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        receive(intent)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putLong(KEY_PICK, pendingPick)
        outState.putBoolean(KEY_PERSIST, pendingPersist)
        outState.putLong(KEY_CREATE, pendingCreate)
        super.onSaveInstanceState(outState)
    }

    override fun onStop() {
        Bridge.stopping()
        super.onStop()
        // The app may be killed from here on: the snapshot of unsaved changes, and documents being written
        // back, are given time to finish first.
        Bridge.awaitSaved(1500)
        Bridge.awaitIo(1500)
    }

    override fun onDestroy() {
        Bridge.detach(this)
        super.onDestroy()
    }

    /** Open with, Edit, Share: the documents (or shared text, as a text file) go to the app. */
    private fun receive(intent: Intent?) {
        intent ?: return
        val uris = when (intent.action) {
            Intent.ACTION_VIEW, Intent.ACTION_EDIT -> listOfNotNull(intent.data)
            Intent.ACTION_SEND -> listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java) ?: sharedText(intent))
            Intent.ACTION_SEND_MULTIPLE -> IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            else -> emptyList()
        }
        Bridge.incoming(uris.filter(::acceptable), intent.flags)
    }

    /**
     * Documents another app may hand over: from a document provider (not this app's own), or the text this
     * activity saved for a share. Never another path in this app's storage.
     */
    private fun acceptable(uri: Uri): Boolean = when (uri.scheme) {
        ContentResolver.SCHEME_CONTENT -> uri.authority != "$packageName.files"
        ContentResolver.SCHEME_FILE -> try {
            val shared = File(cacheDir, "shared").canonicalFile
            uri.path?.let { File(it).canonicalFile.startsWith(shared) } == true
        } catch (e: java.io.IOException) {
            false
        }
        else -> false
    }.also { if (!it) Log.w(TAG, "not opening a document from ${uri.scheme}") }

    /** Text shared without a file: saved as one, which opens as a new document. */
    private fun sharedText(intent: Intent): Uri? {
        val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString() ?: return null
        return try {
            val dir = File(cacheDir, "shared/${SystemClock.elapsedRealtime()}").apply { mkdirs() }
            val file = File(dir, "Shared text.txt")
            file.writeText(text)
            Uri.fromFile(file)
        } catch (e: Exception) {
            Log.w(TAG, "shared text: ${e.javaClass.name}")
            null
        }
    }

    internal fun openDocument(req: Long, mimeTypes: Array<String>, persist: Boolean) {
        pendingPick = req
        pendingPersist = persist
        try {
            openLauncher.launch(mimeTypes)
        } catch (e: RuntimeException) {
            pendingPick = NO_REQUEST
            Bridge.pickFailed(req, e)
        }
    }

    internal fun createDocument(req: Long, name: String, mimeType: String) {
        pendingCreate = req
        try {
            createLauncher.launch(arrayOf(name, mimeType))
        } catch (e: RuntimeException) {
            pendingCreate = NO_REQUEST
            Bridge.pickFailed(req, e)
        }
    }

    /**
     * The keyboard's buffer changed (GameTextInput's listener). Sent to the host tagged with the push it
     * follows; not to GameActivity's native side, whose copy of the text is cut at 64 KiB.
     */
    override fun stateChanged(newState: State, dismissed: Boolean) {
        Bridge.imeState(newState, imeGeneration)
    }

    /** New text for the keyboard (the document around the caret). The keyboard is told where the selection is first. */
    internal fun imePush(text: String, anchor: Int, focus: Int, generation: Long) {
        imeGeneration = generation
        imeEditorInfo?.let {
            it.initialSelStart = anchor
            it.initialSelEnd = focus
        }
        setTextInputState(State(text, anchor, focus, -1, -1))
    }

    /**
     * Keys go straight to the app, also while the on-screen keyboard's connection is open: GameTextInput would
     * otherwise apply them to its own buffer (taking arrows, Tab, Enter and Backspace from a hardware
     * keyboard, losing Shift and Ctrl) and restart the on-screen keyboard after each. The app pushes the
     * keyboard a fresh buffer when the document changes. System keys take the usual way.
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (!event.isSystem) {
            when (event.action) {
                KeyEvent.ACTION_DOWN -> return onKeyDown(event.keyCode, event)
                KeyEvent.ACTION_UP -> return onKeyUp(event.keyCode, event)
            }
        }
        return super.dispatchKeyEvent(event)
    }

    internal fun setPointer(type: Int) {
        if (type == pointerType) return
        pointerType = type
        mSurfaceView?.pointerIcon = PointerIcon.getSystemIcon(this, type)
    }

    internal fun printPdf(file: File, jobName: String) {
        val printManager = getSystemService(PrintManager::class.java) ?: return
        printManager.print(jobName, PdfFileAdapter(file, jobName), null)
    }

    /**
     * GameActivity's insets listener on its surface view. Windows are edge to edge (enforced from Android 15):
     * the surface view's parent is padded, so the app's surface stays clear of the status and navigation
     * bars, display cutouts and the on-screen keyboard.
     */
    override fun onApplyWindowInsets(v: View, insets: WindowInsetsCompat): WindowInsetsCompat {
        val types = WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
        val safe = insets.getInsets(types)
        val parent = v.parent as? View
        if (parent != null && (parent.paddingLeft != safe.left || parent.paddingTop != safe.top || parent.paddingRight != safe.right || parent.paddingBottom != safe.bottom)) {
            Log.i(TAG, "keeping the surface clear of ${safe.left}, ${safe.top}, ${safe.right}, ${safe.bottom} px (left, top, right, bottom)")
            parent.setPadding(safe.left, safe.top, safe.right, safe.bottom)
        }
        return super.onApplyWindowInsets(v, insets)
    }

    /**
     * Back (button or gesture) reaches the app as the Back key, which closes what's open like Esc; with
     * nothing open, the app moves to the background.
     */
    private fun sendBackToApp() {
        val now = SystemClock.uptimeMillis()
        onKeyDown(KeyEvent.KEYCODE_BACK, KeyEvent(now, now, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_BACK, 0))
        onKeyUp(KeyEvent.KEYCODE_BACK, KeyEvent(now, now, KeyEvent.ACTION_UP, KeyEvent.KEYCODE_BACK, 0))
    }

    /** ACTION_OPEN_DOCUMENT for several MIME types; the result intent carries the grant flags. */
    private class OpenDocument : ActivityResultContract<Array<String>, Intent?>() {
        override fun createIntent(context: Context, input: Array<String>): Intent =
            Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType(if (input.size == 1) input[0] else "*/*")
                .putExtra(Intent.EXTRA_MIME_TYPES, input)

        override fun parseResult(resultCode: Int, intent: Intent?): Intent? = if (resultCode == Activity.RESULT_OK) intent else null
    }

    /** ACTION_CREATE_DOCUMENT with the name and MIME type chosen at launch (`[name, mimeType]`). */
    private class CreateDocument : ActivityResultContract<Array<String>, Intent?>() {
        override fun createIntent(context: Context, input: Array<String>): Intent =
            Intent(Intent.ACTION_CREATE_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType(input.getOrNull(1) ?: "application/octet-stream")
                .putExtra(Intent.EXTRA_TITLE, input.getOrNull(0) ?: "")

        override fun parseResult(resultCode: Int, intent: Intent?): Intent? = if (resultCode == Activity.RESULT_OK) intent else null
    }

    /** Prints an existing PDF: one document, every page, copied as is (on its own thread). */
    private class PdfFileAdapter(private val file: File, private val name: String) : PrintDocumentAdapter() {
        private var laidOut = false

        override fun onLayout(oldAttributes: PrintAttributes?, newAttributes: PrintAttributes, cancellationSignal: CancellationSignal?, callback: LayoutResultCallback, extras: Bundle?) {
            if (cancellationSignal?.isCanceled == true) {
                callback.onLayoutCancelled()
                return
            }
            // The PDF doesn't change with the printer's settings.
            callback.onLayoutFinished(PrintDocumentInfo.Builder(name).setContentType(PrintDocumentInfo.CONTENT_TYPE_DOCUMENT).build(), !laidOut)
            laidOut = true
        }

        override fun onWrite(pages: Array<out PageRange>, destination: ParcelFileDescriptor, cancellationSignal: CancellationSignal?, callback: WriteResultCallback) {
            Thread {
                try {
                    FileInputStream(file).use { input -> FileOutputStream(destination.fileDescriptor).use { input.copyTo(it) } }
                    if (cancellationSignal?.isCanceled == true) callback.onWriteCancelled() else callback.onWriteFinished(arrayOf(PageRange.ALL_PAGES))
                } catch (e: Exception) {
                    callback.onWriteFailed(e.javaClass.name)
                }
            }.start()
        }
    }

    companion object {
        private const val TAG = "FolioWriter"
        private const val NO_REQUEST = -1L
        private const val KEY_PICK = "folio.pendingPick"
        private const val KEY_PERSIST = "folio.pendingPersist"
        private const val KEY_CREATE = "folio.pendingCreate"

        /** Launch flags as for the desktop app, e.g. `adb shell am start -n site.lutfi.foliowriter/.MainActivity --es args --sample`. */
        const val EXTRA_ARGS = "args"

        @JvmStatic
        private external fun nativeSetLaunchArgs(args: String)
    }
}
