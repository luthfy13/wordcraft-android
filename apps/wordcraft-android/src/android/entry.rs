//! The two symbols Android finds by name: `android_main`, which GameActivity's native thread calls, and
//! the native method `MainActivity.nativeSetLaunchArgs`.
//!
//! Exporting an unmangled symbol is `unsafe` in Rust 2024 (`#[unsafe(no_mangle)]`), so this module is
//! the crate's one exception to `unsafe_code = "deny"`. It has no `unsafe` block.
#![allow(unsafe_code)]

use std::sync::{Mutex, MutexGuard};

use android_activity::AndroidApp;
use jni::EnvUnowned;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JString};

/// The launching intent's flags, set before the activity starts the native thread.
static LAUNCH_ARGS: Mutex<String> = Mutex::new(String::new());

fn launch_args_lock() -> MutexGuard<'static, String> {
    LAUNCH_ARGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The flags the activity was launched with (`--sample`, `--backend gl`…), or empty.
pub fn launch_args() -> String {
    launch_args_lock().clone()
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    // The panic hook has logged the panic (redacted) with its backtrace. Catching it here keeps
    // android-activity from logging the raw message again; returning finishes the activity either way.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::main(app))).is_err() {
        log::error!("Folio Writer stopped after a panic");
    }
}

/// `MainActivity.nativeSetLaunchArgs(String)`, called before `super.onCreate`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_site_lutfi_foliowriter_MainActivity_nativeSetLaunchArgs<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    args: JString<'caller>,
) {
    env.with_env(|env| -> Result<(), jni::errors::Error> {
        *launch_args_lock() = args.try_to_string(env)?;
        Ok(())
    })
    .resolve::<LogErrorAndDefault>();
}
