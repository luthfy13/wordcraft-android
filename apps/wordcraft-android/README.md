# Folio Writer for Android (`apps/wordcraft-android`)

WordCraft as a native Android app for tablets with a keyboard and a touchpad or mouse, and touch.
Not a WebView and not WASM: the same Rust `WordApp` as the desktop, drawn with wgpu.

- Application id `site.lutfi.foliowriter`, name **Folio Writer**.
- minSdk 29 (Android 10), targetSdk 36 (what Google Play requires since 31 August 2026).
- Ships `arm64-v8a`; CI adds `x86_64` for the emulator.

## Status (phase 1: a minimal build that runs)

Built and checked: unit tests on Linux, clippy for Linux and Android, the debug APK, 16 KB alignment, and
(in CI) an emulator smoke test. Not yet run on a tablet. In this phase:

- Typing needs a hardware keyboard: the on-screen keyboard and IME composition come in phase 2.
- Copy and paste work inside the app only; the system clipboard, file dialogs (the app's own file browser
  opens instead), opening links, sharing and printing come in phase 2.
- Back closes what's open, like Esc; the app doesn't quit on it.

## How it fits together

| Part | What it does |
|---|---|
| `src/host.rs` | Owns egui's context and the `WordApp`; runs a frame the way eframe does (`raw_input_hook`, then `logic` and `ui`). No Android or GPU types: tested on Linux. |
| `src/input.rs`, `src/keymap.rs` | Android touch, mouse, touchpad, stylus and key events → egui events (modifiers, Cut/Copy/Paste keys, text, dead keys). Tested on Linux. |
| `src/android/mod.rs` | The event loop on `android-activity` (GameActivity): window and lifecycle, input, frames only when egui asks for one. |
| `src/android/gfx.rs` | wgpu: Vulkan, falling back to OpenGL ES; every call that can panic is guarded. |
| `src/android/keychars.rs` | The characters keys type, from Android's key character maps (safe JNI). |
| `src/android/entry.rs` | The two exported symbols (`android_main`, `MainActivity.nativeSetLaunchArgs`). |
| `android/` | The Gradle project: a thin Kotlin `MainActivity` (system bars, Back) and the cargo-ndk build. |
| `ci/smoke.sh` | The emulator smoke test run by `.github/workflows/android.yml`. |

Why not eframe: winit's Android backend drops mouse buttons, hover, the wheel, keyboard modifiers and IME
text, which a word processor on a tablet with a keyboard needs. Owning the loop keeps every core crate
unchanged.

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
