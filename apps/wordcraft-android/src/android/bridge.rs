//! The host's calls into Kotlin (`site.lutfi.foliowriter.Bridge`), as `platform.rs` describes them: one
//! method, `call(op: String, args: String): String?`, with JSON arguments and answer.
//!
//! Kotlin's `Bridge` object registers itself with `nativeInit` (`entry.rs`); it is kept as a global
//! reference for the process, so nothing is looked up by class name (a native thread's class loader can't
//! see app classes). Only the safe `jni` API is used. Calls are made on `android_main`'s thread, one at a
//! time, and never from inside another call; a Java exception comes back as an error with its class name
//! only (messages can quote file names).

use std::sync::OnceLock;

use jni::errors::Error as JniError;
use jni::objects::{Global, JObject, JString};
use jni::{Env, JValue, JavaVM, jni_sig, jni_str};
use serde_json::Value;

use crate::platform::Platform;

static BRIDGE: OnceLock<Global<JObject<'static>>> = OnceLock::new();

/// Keeps Kotlin's `Bridge` object (`Bridge.nativeInit`, once per activity; the object lives as long as
/// the process).
pub(crate) fn set_bridge(env: &Env<'_>, bridge: &JObject<'_>) -> jni::errors::Result<()> {
    if bridge.is_null() {
        return Err(JniError::NullPtr("Bridge"));
    }
    if let Some(kept) = BRIDGE.get() {
        if !env.is_same_object(kept, bridge)? {
            log::warn!("a second Bridge object: keeping the first");
        }
        return Ok(());
    }
    let global: Global<JObject<'static>> = env.new_global_ref(bridge)?;
    let _ = BRIDGE.set(global);
    Ok(())
}

/// A JNI error as logged: a Java exception by its class only.
pub(crate) fn describe(e: &JniError) -> String {
    match e {
        JniError::CaughtJavaException { name, .. } => format!("Java exception {name}"),
        other => other.to_string(),
    }
}

/// Android, through the Kotlin bridge.
pub struct AndroidPlatform;

impl Platform for AndroidPlatform {
    fn call(&self, op: &str, args: Value) -> Option<Value> {
        let Some(bridge) = BRIDGE.get() else {
            log::warn!("{op}: the bridge isn't ready");
            return None;
        };
        let vm = JavaVM::singleton().inspect_err(|e| log::warn!("{op}: {}", describe(e))).ok()?;
        let answer = vm.attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
            let (jop, jargs) = (env.new_string(op)?, env.new_string(args.to_string())?);
            let obj = env
                .call_method(
                    bridge.as_obj(),
                    jni_str!("call"),
                    jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                    &[JValue::Object(&jop), JValue::Object(&jargs)],
                )?
                .l()?;
            if obj.is_null() {
                return Ok(None);
            }
            let s = env.cast_local::<JString>(obj)?;
            s.try_to_string(env).map(Some)
        });
        match answer {
            Ok(Some(json)) => serde_json::from_str(&json).inspect_err(|_| log::warn!("{op}: an answer that isn't JSON")).ok(),
            Ok(None) => None,
            Err(e) => {
                log::warn!("{op}: {}", describe(&e));
                None
            }
        }
    }
}
