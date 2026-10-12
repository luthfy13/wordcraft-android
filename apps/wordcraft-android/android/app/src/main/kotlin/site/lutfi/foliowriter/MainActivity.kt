package site.lutfi.foliowriter

import android.os.Bundle
import android.os.SystemClock
import android.view.KeyEvent
import android.view.View
import androidx.activity.OnBackPressedCallback
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import com.google.androidgamesdk.GameActivity

/**
 * The window for the native app (`libwordcraft_android.so`), which draws and handles input itself.
 * This class only does what needs the Android SDK.
 */
class MainActivity : GameActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // GameActivity starts the native thread in super.onCreate, which reads the launch flags at once.
        System.loadLibrary("wordcraft_android")
        nativeSetLaunchArgs(intent?.getStringExtra(EXTRA_ARGS).orEmpty())
        super.onCreate(savedInstanceState)
        keepClearOfSystemBars()
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() = sendBackToApp()
        })
    }

    /**
     * Windows are edge to edge (enforced from Android 15): pad the content so the app's surface stays clear
     * of the status and navigation bars, display cutouts and the on-screen keyboard.
     */
    private fun keepClearOfSystemBars() {
        val content = findViewById<View>(android.R.id.content)
        ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
            val types = WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
            val safe = insets.getInsets(types)
            view.setPadding(safe.left, safe.top, safe.right, safe.bottom)
            insets
        }
    }

    /** Back (button or gesture) reaches the app as the Back key, which closes what's open like Esc. */
    private fun sendBackToApp() {
        val now = SystemClock.uptimeMillis()
        onKeyDown(KeyEvent.KEYCODE_BACK, KeyEvent(now, now, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_BACK, 0))
        onKeyUp(KeyEvent.KEYCODE_BACK, KeyEvent(now, now, KeyEvent.ACTION_UP, KeyEvent.KEYCODE_BACK, 0))
    }

    companion object {
        /** Launch flags as for the desktop app, e.g. `adb shell am start -n site.lutfi.foliowriter/.MainActivity --es args --sample`. */
        const val EXTRA_ARGS = "args"

        @JvmStatic
        private external fun nativeSetLaunchArgs(args: String)
    }
}
