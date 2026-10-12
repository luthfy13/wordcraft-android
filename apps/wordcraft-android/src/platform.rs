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
use crate::ime;

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
    /// The on-screen keyboard's buffer changed; `generation` names the push it was based on.
    Ime {
        generation: u32,
        state: ime::Buf,
    },
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
        FileOp::Refresh { req, uri, path: p, expect } => {
            ("refresh", json!({"req": req, "uri": uri, "path": path(p), "expect": remote_json(*expect)}))
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
            req: req()?,
            result: match error(&v) {
                Some(e) => Err(e),
                None if v.get("changed").and_then(Value::as_bool).unwrap_or(false) => Ok(Some(remote(v.get("remote")))),
                None => Ok(None),
            },
        },
        "ime" => {
            let int = |k: &str| v.get(k).and_then(Value::as_i64);
            let text = v.get("text")?.as_str()?;
            let compose = int("compStart").zip(int("compEnd")).filter(|(a, b)| *a >= 0 && *b >= 0);
            let state = ime::normalize(text, (int("selStart")?, int("selEnd")?), compose);
            let generation = u32::try_from(v.get("generation")?.as_u64()?).ok()?;
            return Some(Inbound::Ime { generation, state });
        }
        "incoming" => {
            let docs: Vec<Picked> = v.get("documents")?.as_array()?.iter().filter_map(picked).take(16).collect();
            return Some(Inbound::Incoming(docs));
        }
        _ => return None,
    };
    Some(Inbound::File(event))
}

/// `onStop` waits (boundedly) for the settings and the recovery snapshot: once it returns, Android may kill
/// the process without another word.
pub mod saved {
    use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, Instant};

    /// Stops asked for, the last one whose files are written, and whether the loop runs (when it doesn't,
    /// nobody writes: there's nothing to wait for).
    static STATE: Mutex<(u64, u64, bool)> = Mutex::new((0, 0, false));
    static DONE: Condvar = Condvar::new();

    fn lock() -> MutexGuard<'static, (u64, u64, bool)> {
        STATE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The loop started (`true`) or ended.
    pub fn set_running(running: bool) {
        let mut s = lock();
        s.2 = running;
        s.1 = s.0;
        DONE.notify_all();
    }

    /// The activity is stopping (UI thread, before the loop hears of it).
    pub fn stopping() {
        let mut s = lock();
        s.0 = s.0.wrapping_add(1);
    }

    /// The loop wrote what a stop needs.
    pub fn done() {
        let mut s = lock();
        s.1 = s.0;
        DONE.notify_all();
    }

    /// Waits up to `timeout` for the stops asked for so far; whether they were done.
    pub fn wait(timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = lock();
        let want = s.0;
        while s.2 && s.1 < want {
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return false;
            }
            s = DONE.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }
}

/// Reports waiting for the `android_main` loop.
pub mod inbox {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    /// Keyboard states kept at most while the loop is busy: the oldest go first (each is the whole buffer, so
    /// a later one supersedes it). Other reports (pickers, copies, write-backs) are never dropped below
    /// `MAX_REPORTS`, far more than the app ever has under way.
    pub const MAX_PENDING: usize = 256;
    pub const MAX_REPORTS: usize = 4096;

    fn is_keyboard(report: &str) -> bool {
        report.contains(r#""event":"ime""#)
    }

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
                let oldest_keyboard = inbox.queue.iter().position(|r| is_keyboard(r));
                let drop = match oldest_keyboard {
                    Some(i) => Some(i),
                    None if inbox.queue.len() >= MAX_REPORTS => Some(0),
                    None => None,
                };
                if let Some(i) = drop {
                    inbox.queue.remove(i);
                    inbox.dropped = inbox.dropped.saturating_add(1);
                }
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
        assert_eq!(parse(r#"{"event":"refreshed","req":2}"#), Some(Inbound::File(FileEvent::Refreshed { req: 2, result: Ok(None) })));
        let Some(Inbound::Incoming(docs)) = parse(r#"{"event":"incoming","documents":[{"uri":"content://a"},{"name":"no uri"}]}"#) else { panic!() };
        assert_eq!(docs.len(), 1);
        let ime = parse(r#"{"event":"ime","text":"a😀b","selStart":3,"selEnd":3,"compStart":-1,"compEnd":-1,"generation":4}"#);
        let Some(Inbound::Ime { generation: 4, state }) = ime else { panic!("{ime:?}") };
        assert_eq!((state.focus, state.compose), (2, None));
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
    fn stopping_waits_for_the_files() {
        use std::time::Duration;
        saved::set_running(false);
        saved::stopping();
        assert!(saved::wait(Duration::from_secs(10)), "no loop: nothing to wait for");
        saved::set_running(true);
        saved::stopping();
        assert!(!saved::wait(Duration::from_millis(20)), "not written yet");
        let writer = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            saved::done();
        });
        assert!(saved::wait(Duration::from_secs(10)));
        writer.join().unwrap();
        assert!(saved::wait(Duration::ZERO), "nothing more to wait for");
    }

    #[test]
    fn the_inbox_wakes_the_loop_and_drops_the_oldest() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&woken);
        inbox::set_waker(Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })));
        let _ = inbox::take();
        inbox::deliver(r#"{"event":"wroteBack","entry":1}"#.into());
        for i in 0..inbox::MAX_PENDING + 2 {
            inbox::deliver(format!(r#"{{"event":"ime","generation":{i}}}"#));
        }
        let (queue, dropped) = inbox::take();
        assert_eq!(queue.len(), inbox::MAX_PENDING);
        assert!(queue.front().is_some_and(|r| r.contains("wroteBack")), "a file report is never dropped for keyboard states");
        assert!(queue.get(1).is_some_and(|r| r.contains(r#""generation":3}"#)), "the oldest keyboard states go");
        assert_eq!(dropped, 3);
        assert!(woken.load(Ordering::Relaxed) >= inbox::MAX_PENDING);
        inbox::set_waker(None);
        inbox::deliver("x".into());
        assert_eq!(inbox::take().0.len(), 1);
    }
}
