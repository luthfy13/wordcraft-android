//! The text keys type, from Android's key character maps over JNI.
//!
//! `KeyCharacterMap.load(device)` is what `KeyEvent.getUnicodeChar` uses: it follows the keyboard's current
//! layout, and falls back to the virtual keyboard's map for a device without one (the built-in keyboard,
//! device 0). Dead keys combine through `KeyCharacterMap.getDeadChar`. Only the safe `jni` API is used, on
//! `android_main`'s thread, which android-activity attached to the VM.

use jni::{Env, JValue, jni_sig, jni_str};

use crate::keymap::{DeadKeys, KeyChar};

#[derive(Default)]
pub struct KeyChars {
    dead: DeadKeys,
}

impl KeyChars {
    /// The text a key press types (a pending dead key's accent included), or `None`.
    pub fn typed(&mut self, device_id: i32, keycode: u32, meta_state: u32) -> Option<String> {
        let ch = java(|env| char_of(env, device_id, keycode, meta_state)).map_or(KeyChar::None, KeyChar::from_java);
        self.dead.press(keycode, ch, |accent, c| {
            java(|env| dead_char(env, accent, c)).and_then(|v| u32::try_from(v).ok()).and_then(char::from_u32).filter(|&c| c != '\0')
        })
    }

    /// Forgets a pending dead key (the window lost focus).
    pub fn clear(&mut self) {
        self.dead.clear();
    }
}

/// Runs `f` with this thread's JNI environment. A Java exception comes back as an error (jni catches and
/// clears it), which is logged.
fn java<T>(f: impl FnOnce(&mut Env<'_>) -> jni::errors::Result<T>) -> Option<T> {
    let vm = jni::JavaVM::singleton().inspect_err(|e| log::warn!("JNI: {e}")).ok()?;
    vm.attach_current_thread(f).inspect_err(|e| log::warn!("key map: {e}")).ok()
}

fn char_of(env: &mut Env<'_>, device_id: i32, keycode: u32, meta_state: u32) -> jni::errors::Result<i32> {
    let map = env
        .call_static_method(
            jni_str!("android/view/KeyCharacterMap"),
            jni_str!("load"),
            jni_sig!("(I)Landroid/view/KeyCharacterMap;"),
            &[JValue::Int(device_id)],
        )?
        .l()?;
    env.call_method(&map, jni_str!("get"), jni_sig!("(II)I"), &[JValue::Int(int(keycode)), JValue::Int(int(meta_state))])?.i()
}

fn dead_char(env: &mut Env<'_>, accent: char, c: char) -> jni::errors::Result<i32> {
    env.call_static_method(
        jni_str!("android/view/KeyCharacterMap"),
        jni_str!("getDeadChar"),
        jni_sig!("(II)I"),
        &[JValue::Int(int(u32::from(accent))), JValue::Int(int(u32::from(c)))],
    )?
    .i()
}

/// Java's `int` for an unsigned value (key codes, meta states and chars all fit).
fn int(v: u32) -> i32 {
    i32::from_ne_bytes(v.to_ne_bytes())
}
