# Folio Writer for Android (`apps/wordcraft-android`)

WordCraft as a native Android app for tablets with a keyboard and a touchpad or mouse, and touch.
Not a WebView and not WASM: the same Rust `WordApp` as the desktop, drawn with wgpu.

- Application id `site.lutfi.foliowriter`, name **Folio Writer**.
- minSdk 29 (Android 10), targetSdk 36 (what Google Play requires since 31 August 2026).
- Ships `arm64-v8a`; CI adds `x86_64` for the emulator.

## Status (phase 2: Android integration)

Built and checked: unit tests on Linux (the host, files, recovery, keyboard and protocol logic), clippy for
Linux and Android, the debug APK, 16 KB alignment, and (in CI) emulator smoke tests on API 34 and 36:
both graphics backends, the system fonts, recovery after a kill, text shared with the app, and a panic
reaching logcat. **Not yet run on a tablet**: the checklist in the phase report covers what needs one.

What works, through Android's own services:

- **Files.** Open, Save As, Export and Save as PDF use the system's document picker. Each document has a
  copy in the app's storage that the app opens and saves as on the desktop (AutoSave included); every save
  is written back to the document, and its permission is kept, so Recent opens it again after a restart.
  A document changed elsewhere asks before it's replaced; one that can't be written (read-only, permission
  gone) stays marked unsaved and asks where to save. Open with, Edit and Share (documents, pictures, text)
  open in the app.
- **No lost work.** Settings (`ui.json`) are written atomically. While a document has unsaved changes a
  snapshot is written (after a pause in editing, at least every 30 s, and when the app leaves the screen,
  which `onStop` waits for); the next start brings it back after a kill or crash. A panic in the interface
  keeps the document and starts the interface again.
- **Typing.** Hardware keyboards as on the desktop (Word's shortcuts, Shift and Ctrl with arrows, dead
  keys). The on-screen keyboard types into the document with composition (CJK, Korean, suggestions,
  AutoCorrect, swipe); a tap on the page or a field shows it.
- **Clipboard** (plain text, both ways; rich formatting stays within the app, as on the desktop),
  **printing** through Android's print service, **sharing** an exported file, **links**, the mouse
  **pointer's shapes**, **Back** (closes what's open; with nothing open the app goes to the background).
- **Fonts and look.** Android's system fonts (CJK, Arabic and the other scripts), a monochrome emoji
  fallback, the interface following the system's light or dark mode until the user picks one.
- **About** shows Folio Writer, the licences of everything it ships and WordCraft's contributors.

Known limits: colour emoji draw in PDFs but not on screen (monochrome there); TalkBack sees the window but
not the app's controls yet; Read Aloud needs a speech engine hook; the interface still says "WordCraft" in
some places.

## How it fits together

| Part | What it does |
|---|---|
| `src/host.rs` | Owns egui's context and the `WordApp`; runs a frame the way eframe does (`raw_input_hook`, then `logic` and `ui`), then hands the platform what the frame asked for. No Android or GPU types: tested on Linux with a fake platform. |
| `src/input.rs`, `src/keymap.rs` | Android touch, mouse, touchpad, stylus and key events → egui events (modifiers, Cut/Copy/Paste keys, text, dead keys). |
| `src/ime.rs` | The on-screen keyboard: the text around the caret it's given, and its edits turned into keys and text. |
| `src/files.rs` | Documents in Android's storage: pickers, copies, write-backs, Recent. |
| `src/recovery.rs`, `src/store.rs` | Recovery snapshots; settings and the app's folders, written atomically. |
| `src/platform.rs` | The JSON calls into Kotlin and the reports back, and the queue the loop drains. |
| `src/about.rs`, `src/prompts.rs` | What the host shows itself: About and licences; the conflict question and the share offer. |
| `src/android/mod.rs` | The event loop on `android-activity` (GameActivity): window and lifecycle, input, frames only when egui asks for one. |
| `src/android/gfx.rs` | wgpu: Vulkan, falling back to OpenGL ES; every call that can panic is guarded. |
| `src/android/bridge.rs`, `src/android/keychars.rs` | Calls into Kotlin's `Bridge` and Android's key character maps (safe JNI only). |
| `src/android/entry.rs` | The exported symbols (`android_main` and the native methods). |
| `android/` | The Gradle project: Kotlin `MainActivity` (pickers, documents sent to the app, printing, keyboard, Back) and `Bridge` (storage, clipboard, links, sharing), and the cargo-ndk build. |
| `licenses/crates.txt` | The Rust crates in the build and their licences, shown in About (`ci/licenses.py`; CI checks it's current). |
| `ci/smoke.sh` | The emulator smoke test run by `.github/workflows/android.yml`. |

Why not eframe: winit's Android backend drops mouse buttons, hover, the wheel, keyboard modifiers and IME
text, which a word processor on a tablet with a keyboard needs. Owning the loop keeps every core crate
unchanged.

**Threads.** Every call into Kotlin is made from the `android_main` thread and returns at once (UI work is
posted to the UI thread, storage work runs on one I/O thread); Kotlin's reports go into a queue and wake
the loop. Neither side waits for the other, except `onStop`, which waits up to 1.5 s for the recovery
snapshot.

**One exception to "no `unsafe`".** The workspace forbids unsafe code. Android finds `android_main` and JNI
methods by name, and exporting an unmangled symbol is `#[unsafe(no_mangle)]` in Rust 2024. So this crate
has its own `[lints]` (the workspace's, with `unsafe_code = "deny"` instead of `forbid`) and one module,
`src/android/entry.rs`, allows it. That module has no `unsafe` block.

## Build

Needs: Rust with the `aarch64-linux-android` (and for the emulator `x86_64-linux-android`) targets,
`cargo install cargo-ndk --version 4.1.2 --locked`, JDK 17 or newer, the Android SDK (platform 36,
build-tools 36.0.0) and NDK `28.2.13676358` (`ANDROID_HOME`, `ANDROID_NDK_HOME`).

```sh
cd apps/wordcraft-android/android
./gradlew assembleDebug                              # arm64-v8a
./gradlew assembleDebug -Pfolio.abis=arm64-v8a,x86_64  # plus the emulator
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Gradle runs `cargo ndk … build --profile android-debug` (or `android-release`); the profiles are in
`.cargo/config.toml` here, so the workspace's own profiles stay as upstream has them. The debug profile is
optimised like a release (typing must be fast) with debug assertions; it keeps the symbol table, so that
a panic's backtrace in logcat can name functions (the smoke test checks it).

Launch flags, as for the desktop app:

```sh
adb shell am start -n site.lutfi.foliowriter/.MainActivity --es args "'--sample'"              # the sample document
adb shell am start -n site.lutfi.foliowriter/.MainActivity --es args "'--sample --backend gl'"  # OpenGL ES only
adb logcat -s FolioWriter                                                                      # the app's log
```

Debug builds also take `--panic-test`, which panics after the first frames so CI can check that a panic
reaches logcat with its backtrace. It's the one deliberate panic outside tests, and release builds don't
contain it.

## Checks

- `cargo test -p wordcraft-android` and `cargo clippy -p wordcraft-android --all-targets -- -D warnings`
  (Linux), part of `cargo xtask ci`.
- `cargo ndk --target arm64-v8a --platform 29 clippy -p wordcraft-android -- -D warnings` lints the
  Android modules, which a Linux build leaves out.
- CI (`.github/workflows/android.yml`) also builds the APK, checks 16 KB page alignment, and runs
  `ci/smoke.sh` on API 34 and 36 emulators.
