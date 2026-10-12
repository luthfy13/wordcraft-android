//! What the host asks of Android and what Android reports back, as JSON: one call into Kotlin
//! (`Bridge.call(op, args)`) and one call back (`Bridge.nativeEvent(json)`). Two JNI entry points, with
//! the messages parsed and tested here on any host. Android's side is `android/bridge.rs` and
//! `Bridge.kt`.
//!
//! Reports arrive on Android's UI thread or Kotlin's I/O thread and wait in the [`inbox`] for the
//! `android_main` loop, which a waker wakes.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::files::{FileEvent, FileOp, Picked, Remote, WriteError};

/// Calls into the platform, made on the `android_main` thread. Kotlin's methods return at once (UI work
/// is posted to the UI thread; I/O runs on its own thread) and never call back synchronously.
pub trait Platform {
    /// Runs `op` with `args`; its answer, if it has one. `None` also when the call failed (logged).
    fn call(&self, op: &str, args: Value) -> Option<Value>;
}

/// A report from the platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    File(FileEvent),
    /// The app was asked to open these (Open with, Share): the documents, copied in by [`FileEvent`]s.
    Incoming(Vec<Picked>),
}

/// The call for a file operation.
pub fn file_op(op: &FileOp) -> (&'static str, Value) {
    let path = |p: &PathBuf| p.to_string_lossy().into_owned();
    match op {
        FileOp::Pick { req, create, mimes, persist } => (
            "pick",
            json!({
                "req": req,
                "create": create.as_ref().map(|(name, mime)| json!({"name": name, "mime": mime})),
                "mimes": mimes,
                "persist": persist,
            }),
        ),
        FileOp::CopyIn { req, uri, path: p } => ("copyIn", json!({"req": req, "uri": uri, "path": path(p)})),
        FileOp::WriteBack { entry, uri, path: p, expect, force } => {
            ("writeBack", json!({"entry": entry, "uri": uri, "path": path(p), "expect": remote_json(*expect), "force": force}))
        }
        FileOp::Refresh { entry, uri, path: p, expect } => {
            ("refresh", json!({"entry": entry, "uri": uri, "path": path(p), "expect": remote_json(*expect)}))
        }
        FileOp::ListGrants => ("grants", json!({})),
        FileOp::Release { uri } => ("release", json!({"uri": uri})),
    }
}

/// Android's `PointerIcon` type for egui's pointer shape.
pub fn pointer_icon(icon: egui::CursorIcon) -> i32 {
    use egui::CursorIcon as C;
    match icon {
        C::None => 0,
        C::ContextMenu => 1001,
        C::PointingHand => 1002,
        C::Help => 1003,
        C::Wait | C::Progress => 1004,
        C::Cell => 1006,
        C::Crosshair => 1007,
        C::Text => 1008,
        C::VerticalText => 1009,
        C::Alias => 1010,
        C::Copy => 1011,
        C::NoDrop | C::NotAllowed => 1012,
        C::Move | C::AllScroll => 1013,
        C::ResizeHorizontal | C::ResizeEast | C::ResizeWest | C::ResizeColumn => 1014,
        C::ResizeVertical | C::ResizeNorth | C::ResizeSouth | C::ResizeRow => 1015,
        C::ResizeNeSw | C::ResizeNorthEast | C::ResizeSouthWest => 1016,
        C::ResizeNwSe | C::ResizeNorthWest | C::ResizeSouthEast => 1017,
        C::ZoomIn => 1018,
        C::ZoomOut => 1019,
        C::Grab => 1020,
        C::Grabbing => 1021,
        // The arrow (`Default`, and any shape egui adds later).
        _ => 1000,
    }
}

/// The answer to `grants`: the URIs whose permission is kept.
pub fn grants(answer: &Value) -> Option<Vec<String>> {
    Some(answer.get("uris")?.as_array()?.iter().filter_map(Value::as_str).map(str::to_owned).collect())
}

fn remote_json(r: Remote) -> Value {
    json!({"modified": r.modified, "size": r.size})
}

fn remote(v: Option<&Value>) -> Remote {
    let v = v.unwrap_or(&Value::Null);
    Remote { modified: v.get("modified").and_then(Value::as_i64), size: v.get("size").and_then(Value::as_i64) }
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn picked(v: &Value) -> Option<Picked> {
    Some(Picked {
        uri: string(v, "uri")?,
        name: string(v, "name").unwrap_or_default(),
        mime: string(v, "mime"),
        writable: v.get("writable").and_then(Value::as_bool).unwrap_or(false),
        persisted: v.get("persisted").and_then(Value::as_bool).unwrap_or(false),
        is_virtual: v.get("virtual").and_then(Value::as_bool).unwrap_or(false),
        remote: remote(v.get("remote")),
    })
}

/// An error's text: the platform sends an exception's class, never its message (it may quote names).
fn error(v: &Value) -> Option<String> {
    string(v, "error").map(|e| if e.is_empty() { "unknown error".to_owned() } else { e })
}

/// Parses a report; `None` for one this version doesn't know or that's malformed (logged by the caller).
pub fn parse(json: &str) -> Option<Inbound> {
    let v: Value = serde_json::from_str(json).ok()?;
    let req = || v.get("req").and_then(Value::as_u64);
    let entry = || v.get("entry").and_then(Value::as_u64);
    let event = match v.get("event")?.as_str()? {
        "picked" => FileEvent::Picked {
            req: req()?,
            result: match error(&v) {
                Some(e) => Err(e),
                None if v.get("cancelled").and_then(Value::as_bool).unwrap_or(false) => Ok(None),
                None => Ok(Some(picked(&v)?)),
            },
        },
        "copiedIn" => FileEvent::CopiedIn { req: req()?, result: error(&v).map_or_else(|| Ok(remote(v.get("remote"))), Err) },
        "wroteBack" => FileEvent::WroteBack {
            entry: entry()?,
            result: match (
                error(&v),
                v.get("conflict").and_then(Value::as_bool).unwrap_or(false),
                v.get("lost").and_then(Value::as_bool).unwrap_or(false),
            ) {
                (_, true, _) => Err(WriteError::Conflict(remote(v.get("remote")))),
                (Some(e), _, true) => Err(WriteError::Lost(e)),
                (Some(e), _, false) => Err(WriteError::Failed(e)),
                (None, ..) => Ok(remote(v.get("remote"))),
            },
        },
        "refreshed" => FileEvent::Refreshed {
            entry: entry()?,
            result: match error(&v) {
                Some(e) => Err(e),
                None if v.get("changed").and_then(Value::as_bool).unwrap_or(false) => Ok(Some(remote(v.get("remote")))),
                None => Ok(None),
            },
        },
        "incoming" => {
            let docs: Vec<Picked> = v.get("documents")?.as_array()?.iter().filter_map(picked).take(16).collect();
            return Some(Inbound::Incoming(docs));
        }
        _ => return None,
    };
    Some(Inbound::File(event))
}

/// Reports waiting for the `android_main` loop.
pub mod inbox {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    /// Reports kept at most; the oldest go first (an IME's states, say, when the loop is stuck).
    pub const MAX_PENDING: usize = 256;

    type Waker = Arc<dyn Fn() + Send + Sync>;

    struct Inbox {
        queue: VecDeque<String>,
        waker: Option<Waker>,
        dropped: u64,
    }

    static INBOX: Mutex<Inbox> = Mutex::new(Inbox { queue: VecDeque::new(), waker: None, dropped: 0 });

    fn lock() -> MutexGuard<'static, Inbox> {
        INBOX.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The loop's waker (`None` once it has stopped). Reports that arrived before it are picked up.
    pub fn set_waker(waker: Option<Waker>) {
        let wake = waker.clone();
        lock().waker = waker;
        if let Some(wake) = wake {
            wake();
        }
    }

    /// From any thread: queues a report and wakes the loop. Never waits on the loop.
    pub fn deliver(report: String) {
        let wake = {
            let mut inbox = lock();
            if inbox.queue.len() >= MAX_PENDING {
                inbox.queue.pop_front();
                inbox.dropped = inbox.dropped.saturating_add(1);
            }
            inbox.queue.push_back(report);
            inbox.waker.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }

    /// The reports waiting, oldest first, and how many were dropped since the last call.
    pub fn take() -> (VecDeque<String>, u64) {
        let mut inbox = lock();
        let dropped = std::mem::take(&mut inbox.dropped);
        (std::mem::take(&mut inbox.queue), dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn reports_parse() {
        let p = parse(
            r#"{"event":"picked","req":4,"uri":"content://x/1","name":"A.docx","writable":true,"persisted":true,"remote":{"modified":5,"size":null}}"#,
        );
        let Some(Inbound::File(FileEvent::Picked { req: 4, result: Ok(Some(p)) })) = p else { panic!("{p:?}") };
        assert_eq!((p.uri.as_str(), p.name.as_str(), p.writable, p.persisted), ("content://x/1", "A.docx", true, true));
        assert_eq!(p.remote, Remote { modified: Some(5), size: None });
        assert_eq!(parse(r#"{"event":"picked","req":4,"cancelled":true}"#), Some(Inbound::File(FileEvent::Picked { req: 4, result: Ok(None) })));
        assert_eq!(
            parse(r#"{"event":"picked","req":4,"error":"android.content.ActivityNotFoundException"}"#),
            Some(Inbound::File(FileEvent::Picked { req: 4, result: Err("android.content.ActivityNotFoundException".into()) }))
        );
        assert_eq!(
            parse(r#"{"event":"wroteBack","entry":2,"conflict":true,"remote":{"modified":9}}"#),
            Some(Inbound::File(FileEvent::WroteBack { entry: 2, result: Err(WriteError::Conflict(Remote { modified: Some(9), size: None })) }))
        );
        assert_eq!(
            parse(r#"{"event":"wroteBack","entry":2,"error":"java.lang.SecurityException","lost":true}"#),
            Some(Inbound::File(FileEvent::WroteBack { entry: 2, result: Err(WriteError::Lost("java.lang.SecurityException".into())) }))
        );
        assert_eq!(parse(r#"{"event":"refreshed","entry":2}"#), Some(Inbound::File(FileEvent::Refreshed { entry: 2, result: Ok(None) })));
        let Some(Inbound::Incoming(docs)) = parse(r#"{"event":"incoming","documents":[{"uri":"content://a"},{"name":"no uri"}]}"#) else { panic!() };
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn hostile_reports_are_ignored() {
        for bad in [
            "",
            "null",
            "[]",
            "{}",
            r#"{"event":7}"#,
            r#"{"event":"picked"}"#,
            r#"{"event":"picked","req":-1}"#,
            r#"{"event":"nope","req":1}"#,
            "\u{0}{",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn calls_carry_what_kotlin_reads() {
        let (op, args) = file_op(&FileOp::Pick { req: 3, create: Some(("a.pdf".into(), "application/pdf".into())), mimes: vec![], persist: true });
        assert_eq!(op, "pick");
        assert_eq!(args, json!({"req": 3, "create": {"name": "a.pdf", "mime": "application/pdf"}, "mimes": [], "persist": true}));
        let (op, args) = file_op(&FileOp::WriteBack { entry: 1, uri: "u".into(), path: "/p".into(), expect: Remote::default(), force: false });
        assert_eq!(op, "writeBack");
        assert_eq!(args["expect"], json!({"modified": null, "size": null}));
        assert_eq!(grants(&json!({"uris": ["a", 1, "b"]})), Some(vec!["a".to_owned(), "b".to_owned()]));
    }

    #[test]
    fn the_inbox_wakes_the_loop_and_drops_the_oldest() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&woken);
        inbox::set_waker(Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })));
        let _ = inbox::take();
        for i in 0..inbox::MAX_PENDING + 2 {
            inbox::deliver(i.to_string());
        }
        let (queue, dropped) = inbox::take();
        assert_eq!(queue.len(), inbox::MAX_PENDING);
        assert_eq!(queue.front().map(String::as_str), Some("2"));
        assert_eq!(dropped, 2);
        assert!(woken.load(Ordering::Relaxed) >= inbox::MAX_PENDING);
        inbox::set_waker(None);
        inbox::deliver("x".into());
        assert_eq!(inbox::take().0.len(), 1);
    }
}
