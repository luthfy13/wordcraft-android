//! The symbols Android finds by name: `android_main`, which GameActivity's native thread calls, and the
//! native methods `MainActivity.nativeSetLaunchArgs` and `Bridge.native*`.
//!
//! Exporting an unmangled symbol is `unsafe` in Rust 2024 (`#[unsafe(no_mangle)]`), so this module is
//! the crate's one exception to `unsafe_code = "deny"`. It has no `unsafe` block.
#![allow(unsafe_code)]

use std::sync::{Mutex, MutexGuard};

use android_activity::AndroidApp;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::{EnvUnowned, Outcome};

use crate::platform::{inbox, saved};

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

/// Logs a native method's failure: its own text for JNI errors, a Java exception's class only.
fn settle<T: Default>(what: &str, outcome: Outcome<T, jni::errors::Error>) -> T {
    match outcome {
        Outcome::Ok(v) => v,
        Outcome::Err(e) => {
            log::warn!("{what}: {}", super::bridge::describe(&e));
            T::default()
        }
        // The panic hook logged it.
        Outcome::Panic(_) => T::default(),
    }
}

/// `Bridge.nativeInit()`, from each activity's `onCreate`: the object the host calls.
#[unsafe(no_mangle)]
pub extern "system" fn Java_site_lutfi_foliowriter_Bridge_nativeInit<'caller>(mut env: EnvUnowned<'caller>, this: JObject<'caller>) {
    let outcome = env.with_env(|env| super::bridge::set_bridge(env, &this)).into_outcome();
    settle("Bridge.nativeInit", outcome);
}

/// `Bridge.nativeEvent(String)`, from the UI thread or Kotlin's I/O thread: a report (JSON) for the
/// loop, which is woken. Never waits on the loop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_site_lutfi_foliowriter_Bridge_nativeEvent<'caller>(
    mut env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    json: JString<'caller>,
) {
    let outcome = env
        .with_env(|env| -> Result<(), jni::errors::Error> {
            if !json.is_null() {
                inbox::deliver(json.try_to_string(env)?);
            }
            Ok(())
        })
        .into_outcome();
    settle("Bridge.nativeEvent", outcome);
}

/// `Bridge.nativeStopping()`, from `onStop` before GameActivity tells the loop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_site_lutfi_foliowriter_Bridge_nativeStopping<'caller>(_env: EnvUnowned<'caller>, _this: JObject<'caller>) {
    saved::stopping();
}

/// `Bridge.nativeAwaitSaved(int)`, from `onStop`: waits up to that many milliseconds for the loop to write
/// the settings and the recovery snapshot.
#[unsafe(no_mangle)]
pub extern "system" fn Java_site_lutfi_foliowriter_Bridge_nativeAwaitSaved<'caller>(
    _env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    ms: jni::sys::jint,
) {
    let ms = u64::try_from(ms).unwrap_or(0).min(5_000);
    if !saved::wait(std::time::Duration::from_millis(ms)) {
        log::warn!("stopping before the recovery snapshot was written");
    }
}
