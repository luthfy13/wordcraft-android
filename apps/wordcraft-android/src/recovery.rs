//! Unsaved changes survive the app being killed or crashing: while the document has unsaved changes, a
//! copy of it is kept in the app's storage, and the next start opens that copy.
//!
//! Android kills apps in the background without warning, and a native crash ends the process, so the copy
//! is written as the user works: once edits pause for [`IDLE`], at least every [`EVERY`] while they don't,
//! and when the app leaves the screen. It's a `.docx` (with the document's password, if it has one),
//! written on its own thread: the document is cloned (cheap, its blocks are shared) and the main thread
//! carries on.
//!
//! The folder holds `snapshot-<n>.docx` and `manifest.json`, which names the snapshot and the document's
//! own file. The manifest is written after its snapshot, so it never names a snapshot that isn't complete.
//! Saving the document, or closing it without saving, deletes both.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wordcraft_doc::Document;
use wordcraft_engine::io::{ExportOptions, Password};
use wordcraft_ui_egui::WordApp;

use crate::store::write_atomic;

/// Edits pause this long before a snapshot.
pub const IDLE: Duration = Duration::from_secs(2);
/// Edits that don't pause are snapshotted this often.
pub const EVERY: Duration = Duration::from_secs(30);
const MANIFEST: &str = "manifest.json";
const VERSION: u64 = 1;
/// Largest `manifest.json` read.
const MAX_MANIFEST: u64 = 64 << 10;

/// What a snapshot holds, as `manifest.json` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Manifest {
    /// The snapshot's file name, in the recovery folder.
    snapshot: String,
    /// The document's own file, if it has one.
    path: Option<PathBuf>,
    encrypted: bool,
}

impl Manifest {
    fn to_json(&self) -> Value {
        json!({
            "version": VERSION,
            "snapshot": self.snapshot,
            "path": self.path.as_ref().map(|p| p.to_string_lossy()),
            "encrypted": self.encrypted,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        if v.get("version")?.as_u64()? != VERSION {
            return None;
        }
        let snapshot = v.get("snapshot")?.as_str()?.to_owned();
        // Only a file in the recovery folder.
        if !is_snapshot_name(&snapshot) {
            return None;
        }
        let path = v.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(PathBuf::from);
        Some(Self { snapshot, path, encrypted: v.get("encrypted").and_then(Value::as_bool).unwrap_or(false) })
    }
}

fn is_snapshot_name(name: &str) -> bool {
    name.strip_prefix("snapshot-").and_then(|n| n.strip_suffix(".docx")).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

enum Job {
    Write { seq: u64, doc: Box<Document>, password: Option<Password>, manifest: Manifest },
    Clear { seq: u64 },
}

impl Job {
    fn seq(&self) -> u64 {
        match self {
            Job::Write { seq, .. } | Job::Clear { seq } => *seq,
        }
    }
}

/// The last job the writer finished.
#[derive(Default)]
struct Done {
    seq: Mutex<u64>,
    cond: Condvar,
}

/// Keeps the snapshot of the open document up to date.
pub struct Recovery {
    dir: PathBuf,
    tx: Option<Sender<Job>>,
    done: Arc<Done>,
    seq: u64,
    /// The document and revision the newest snapshot holds (or will, once written).
    written: Option<(u64, u64)>,
    /// A snapshot (or its manifest) may be on disk.
    on_disk: bool,
    /// The revision seen last, and when it was first seen.
    seen: Option<((u64, u64), Instant)>,
    /// Since when the document has changes no snapshot holds.
    unsnapshotted: Option<Instant>,
    /// A password-protected snapshot being opened through the app's password dialog: the document it
    /// replaces, the manifest and the snapshot.
    restoring: Option<(u64, Manifest, PathBuf)>,
    /// The password dialog was cancelled: the snapshot stays until a new one replaces it.
    hold: bool,
}

impl Recovery {
    pub fn new(dir: PathBuf) -> Self {
        let on_disk = fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some());
        Self { dir, tx: None, done: Arc::default(), seq: 0, written: None, on_disk, seen: None, unsnapshotted: None, restoring: None, hold: false }
    }

    /// Opens the snapshot a previous run left, if there's one and the document wasn't saved after it.
    /// Returns what to tell the user.
    pub fn restore(&mut self, app: &mut WordApp) -> Option<String> {
        let Some(manifest) = self.read_manifest() else {
            // Leftovers of a write that was killed.
            self.clear_now();
            return None;
        };
        let snapshot = self.dir.join(&manifest.snapshot);
        let Ok(snapshot_time) = fs::metadata(&snapshot).and_then(|m| m.modified()) else {
            self.clear_now();
            return None;
        };
        // The document's file saved after the snapshot holds the newer text.
        if let Some(path) = &manifest.path
            && fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t > snapshot_time)
        {
            log::info!("the document was saved after its recovery snapshot: discarding the snapshot");
            self.clear_now();
            return None;
        }
        let name = manifest.path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
        if manifest.encrypted {
            // The password dialog opens it; `observe` finishes the job.
            let before = app.session.document_id();
            let _ = app.run("file.open", json!({"path": snapshot.to_string_lossy()}));
            self.restoring = Some((before, manifest, snapshot));
            return Some(recovered_message(name.as_deref()));
        }
        match wordcraft_engine::io::open_path(&snapshot) {
            Ok(doc) => {
                app.session.set_document(doc);
                app.session.path = manifest.path;
                app.session.touch();
                self.written = Some((app.session.document_id(), app.session.rev()));
                Some(recovered_message(name.as_deref()))
            }
            Err(e) => {
                log::error!("can't open the recovery snapshot: {e}");
                self.clear_now();
                None
            }
        }
    }

    fn read_manifest(&self) -> Option<Manifest> {
        let path = self.dir.join(MANIFEST);
        let meta = fs::metadata(&path).ok()?;
        if meta.len() > MAX_MANIFEST {
            return None;
        }
        let v: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        let manifest = Manifest::from_json(&v);
        if manifest.is_none() {
            log::warn!("the recovery manifest isn't valid: ignoring it");
        }
        manifest
    }

    /// After each frame and each wake-up: writes a snapshot when one is due, deletes it once the document
    /// is saved. Returns when to look again.
    pub fn observe(&mut self, app: &mut WordApp, now: Instant) -> Option<Instant> {
        self.finish_restoring(app);
        let key = (app.session.document_id(), app.session.rev());
        if !app.session.dirty {
            self.seen = None;
            self.unsnapshotted = None;
            if self.on_disk && self.restoring.is_none() && !self.hold {
                self.clear();
            }
            return None;
        }
        if self.written == Some(key) {
            self.unsnapshotted = None;
            return None;
        }
        let changed = match self.seen {
            Some((k, at)) if k == key => at,
            _ => {
                self.seen = Some((key, now));
                now
            }
        };
        let since = *self.unsnapshotted.get_or_insert(now);
        let due = changed.checked_add(IDLE).into_iter().chain(since.checked_add(EVERY)).min()?;
        if now >= due {
            self.write(app);
            None
        } else {
            Some(due)
        }
    }

    /// Writes a snapshot now if the document has changes none holds (the app is leaving the screen).
    pub fn flush(&mut self, app: &mut WordApp) {
        self.finish_restoring(app);
        if app.session.dirty && self.written != Some((app.session.document_id(), app.session.rev())) {
            self.write(app);
        }
    }

    /// The user closed or quit, saving or not: nothing to recover.
    pub fn discard(&mut self) {
        self.restoring = None;
        self.hold = false;
        self.clear();
    }

    /// Waits up to `timeout` for the writer to finish; whether it did.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut done = self.done.seq.lock().unwrap_or_else(PoisonError::into_inner);
        while *done < self.seq {
            let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return false;
            }
            done = self.done.cond.wait_timeout(done, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }

    /// A password-protected snapshot was opened through the app: it gets its document's file back.
    fn finish_restoring(&mut self, app: &mut WordApp) {
        let Some((before, _, snapshot)) = &self.restoring else { return };
        if app.session.document_id() == *before {
            // Still asking for the password (or cancelled: a later document change ends it below).
            if app.dialog.is_some() {
                return;
            }
        }
        let opened = app.session.document_id() != *before && app.session.path.as_deref() == Some(snapshot.as_path());
        let Some((_, manifest, snapshot)) = self.restoring.take() else { return };
        let snapshot = snapshot.to_string_lossy().into_owned();
        app.ui.recent.retain(|r| *r != snapshot);
        if opened {
            app.session.path = manifest.path;
            app.session.touch();
        } else {
            log::info!("the recovered document wasn't opened: keeping its snapshot");
            self.hold = true;
        }
    }

    fn write(&mut self, app: &WordApp) {
        let s = &app.session;
        self.seq = self.seq.wrapping_add(1).max(1);
        let manifest = Manifest { snapshot: format!("snapshot-{}.docx", self.seq), path: s.path.clone(), encrypted: s.password.is_some() };
        let job = Job::Write { seq: self.seq, doc: Box::new(s.doc.clone()), password: s.password.clone(), manifest };
        self.written = Some((s.document_id(), s.rev()));
        self.unsnapshotted = None;
        self.on_disk = true;
        self.hold = false;
        self.send(job);
    }

    fn clear(&mut self) {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.written = None;
        self.on_disk = false;
        self.send(Job::Clear { seq: self.seq });
    }

    /// Deletes the snapshot on this thread (at start, before the writer exists).
    fn clear_now(&mut self) {
        clear_dir(&self.dir);
        self.written = None;
        self.on_disk = false;
    }

    fn send(&mut self, job: Job) {
        if self.tx.is_none() {
            self.tx = spawn_writer(self.dir.clone(), Arc::clone(&self.done));
        }
        let job = match &self.tx {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        // No thread: write here.
        self.tx = None;
        let seq = job.seq();
        run(&self.dir, job);
        mark_done(&self.done, seq);
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        // The writer finishes what it has (the channel closes), up to a bound: a snapshot half written
        // leaves the previous one in place.
        self.tx = None;
        self.wait(Duration::from_secs(2));
    }
}

fn recovered_message(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("Recovered unsaved changes to {name}"),
        None => "Recovered an unsaved document".to_owned(),
    }
}

fn spawn_writer(dir: PathBuf, done: Arc<Done>) -> Option<Sender<Job>> {
    let (tx, rx) = mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new().name("recovery".into()).spawn(move || writer(&dir, &rx, &done));
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            log::error!("can't start the recovery writer: {e}");
            None
        }
    }
}

/// Takes the newest job each time: one queued behind it is out of date.
fn writer(dir: &Path, rx: &Receiver<Job>, done: &Done) {
    while let Ok(mut job) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        let seq = job.seq();
        // A panic while writing must not take the app down with it (the writer goes on).
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(dir, job))).is_err() {
            log::error!("writing the recovery snapshot panicked");
        }
        mark_done(done, seq);
    }
}

fn mark_done(done: &Done, seq: u64) {
    let mut d = done.seq.lock().unwrap_or_else(PoisonError::into_inner);
    *d = (*d).max(seq);
    done.cond.notify_all();
}

fn run(dir: &Path, job: Job) {
    match job {
        Job::Write { doc, password, manifest, .. } => {
            let started = Instant::now();
            if let Err(e) = write_snapshot(dir, &doc, password.as_ref(), &manifest) {
                log::error!("can't write the recovery snapshot: {e}");
                return;
            }
            log::info!("recovery snapshot written in {} ms", started.elapsed().as_millis());
            remove_snapshots(dir, Some(&manifest.snapshot));
        }
        Job::Clear { .. } => clear_dir(dir),
    }
}

fn write_snapshot(dir: &Path, doc: &Document, password: Option<&Password>, manifest: &Manifest) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let snapshot = dir.join(&manifest.snapshot);
    wordcraft_engine::io::save_path_with(&snapshot, doc, password.map(Password::as_str), &ExportOptions::default())?;
    let bytes = serde_json::to_vec(&manifest.to_json()).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(MANIFEST), &bytes).map_err(|e| e.to_string())
}

/// The manifest first, so no manifest names a missing snapshot.
fn clear_dir(dir: &Path) {
    match fs::remove_file(dir.join(MANIFEST)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("can't delete the recovery manifest: {e}"),
    }
    remove_snapshots(dir, None);
}

/// Deletes the snapshots but `keep`, and temporary files a killed write left.
fn remove_snapshots(dir: &Path, keep: Option<&str>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stale = (is_snapshot_name(&name) && keep != Some(&*name)) || (name.starts_with('.') && name.ends_with(".tmp"));
        if stale && let Err(e) = fs::remove_file(entry.path()) {
            log::warn!("can't delete {name}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::temp_dir;
    use std::time::SystemTime;
    use wordcraft_doc::StoryRef;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::Services;

    fn app() -> WordApp {
        WordApp::new(Session::new(Document::new()), Services::default())
    }

    fn type_text(app: &mut WordApp, text: &str) {
        app.run("text.insert", json!({"text": text})).unwrap();
        assert!(app.session.dirty);
    }

    fn body(app: &WordApp) -> String {
        app.session.doc.plain_text(StoryRef::Body)
    }

    /// When the snapshot was written.
    fn snapshot_time(dir: &Path) -> Option<SystemTime> {
        let m: Value = serde_json::from_slice(&fs::read(dir.join(MANIFEST)).ok()?).ok()?;
        fs::metadata(dir.join(Manifest::from_json(&m)?.snapshot)).ok()?.modified().ok()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn snapshots_wait_for_a_pause_in_editing() {
        let dir = temp_dir("rec-pause");
        let mut rec = Recovery::new(dir.clone());
        let mut app = app();
        let t0 = Instant::now();
        assert_eq!(rec.observe(&mut app, t0), None, "nothing to keep");
        type_text(&mut app, "Hello");
        assert_eq!(rec.observe(&mut app, t0), Some(t0 + IDLE));
        // Still typing: the wait starts again, up to `EVERY` after the first change.
        let mut t = t0;
        for i in 0..14 {
            t = t0 + Duration::from_secs(i * 2 + 1);
            type_text(&mut app, "!");
            assert_eq!(rec.observe(&mut app, t), Some(t + IDLE), "typing at {t:?}");
        }
        t += Duration::from_secs(1);
        type_text(&mut app, "!");
        assert_eq!(rec.observe(&mut app, t), Some(t0 + EVERY));
        assert_eq!(rec.observe(&mut app, t0 + EVERY), None, "written");
        assert!(rec.wait(Duration::from_secs(10)));
        assert_eq!(files(&dir), ["manifest.json", "snapshot-1.docx"]);
        // Nothing new: nothing written.
        assert_eq!(rec.observe(&mut app, t0 + EVERY + IDLE), None);
        assert_eq!(rec.seq, 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsaved_changes_come_back_after_a_kill() {
        let dir = temp_dir("rec-kill");
        let doc_path = dir.join("letter.docx");
        let mut first = app();
        first.run("file.save", json!({"path": doc_path.to_string_lossy()})).unwrap();
        type_text(&mut first, "Dear Ayu,");
        let mut rec = Recovery::new(dir.join("recovery"));
        rec.flush(&mut first);
        assert!(rec.wait(Duration::from_secs(10)));
        // The process dies here: nothing else runs.
        std::mem::forget(rec);

        let mut second = app();
        let mut rec = Recovery::new(dir.join("recovery"));
        let message = rec.restore(&mut second).expect("recovered");
        assert_eq!(message, "Recovered unsaved changes to letter.docx");
        assert!(body(&second).contains("Dear Ayu,"));
        assert_eq!(second.session.path.as_deref(), Some(doc_path.as_path()), "saving goes to the document's own file");
        assert!(second.session.dirty, "the changes aren't saved yet");
        // The restored state is already in the snapshot: nothing is written again.
        assert_eq!(rec.observe(&mut second, Instant::now()), None);
        assert_eq!(rec.seq, 0);
        // Saving deletes it.
        second.run("file.save", json!({})).unwrap();
        assert_eq!(rec.observe(&mut second, Instant::now()), None);
        assert!(rec.wait(Duration::from_secs(10)));
        assert!(files(&dir.join("recovery")).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_document_saved_after_its_snapshot_wins() {
        let dir = temp_dir("rec-saved");
        let doc_path = dir.join("notes.docx");
        let mut first = app();
        type_text(&mut first, "old text");
        first.session.path = Some(doc_path.clone());
        let mut rec = Recovery::new(dir.join("recovery"));
        rec.flush(&mut first);
        assert!(rec.wait(Duration::from_secs(10)));
        std::mem::forget(rec);
        let snapshot = snapshot_time(&dir.join("recovery")).unwrap();
        // Saved (by another copy of the app, say) after the snapshot.
        std::thread::sleep(Duration::from_millis(20));
        first.run("file.save", json!({})).unwrap();
        let saved = fs::metadata(&doc_path).unwrap().modified().unwrap();
        assert!(saved > snapshot, "the file system keeps sub-second times");

        let mut second = app();
        let mut rec = Recovery::new(dir.join("recovery"));
        assert_eq!(rec.restore(&mut second), None);
        assert!(body(&second).trim().is_empty());
        assert!(files(&dir.join("recovery")).is_empty(), "the stale snapshot is gone");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_untitled_document_comes_back_untitled() {
        let dir = temp_dir("rec-untitled");
        let mut first = app();
        type_text(&mut first, "draft");
        let mut rec = Recovery::new(dir.clone());
        rec.flush(&mut first);
        drop(rec);
        let mut second = app();
        let mut rec = Recovery::new(dir.clone());
        assert_eq!(rec.restore(&mut second).as_deref(), Some("Recovered an unsaved document"));
        assert_eq!(second.session.path, None);
        assert!(body(&second).contains("draft"));
        // Closing without saving: nothing to recover next time.
        rec.discard();
        assert!(rec.wait(Duration::from_secs(10)));
        assert!(files(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_password_protected_document_asks_for_its_password() {
        let dir = temp_dir("rec-password");
        let doc_path = dir.join("secret.docx");
        let mut first = app();
        type_text(&mut first, "top secret");
        first.session.path = Some(doc_path.clone());
        first.session.password = Some(Password::new("pw"));
        let mut rec = Recovery::new(dir.join("recovery"));
        rec.flush(&mut first);
        drop(rec);
        let snapshot = dir.join("recovery").join("snapshot-1.docx");
        assert!(wordcraft_engine::io::open_path(&snapshot).is_err(), "the snapshot is encrypted");

        let mut second = app();
        let mut rec = Recovery::new(dir.join("recovery"));
        assert!(rec.restore(&mut second).is_some());
        assert!(second.dialog.is_some(), "the password dialog is open");
        assert_eq!(rec.observe(&mut second, Instant::now()), None);
        // The user types the password (what the dialog's OK runs).
        second.dialog = None;
        second.run("file.open", json!({"path": snapshot.to_string_lossy(), "password": "pw"})).unwrap();
        rec.observe(&mut second, Instant::now());
        assert!(body(&second).contains("top secret"));
        assert_eq!(second.session.path.as_deref(), Some(doc_path.as_path()));
        assert!(second.session.dirty);
        assert!(!second.ui.recent.iter().any(|r| r.contains("snapshot")), "the snapshot isn't a recent document");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hostile_manifests_are_ignored() {
        let dir = temp_dir("rec-hostile");
        for bad in [
            json!({"version": 1, "snapshot": "../ui.json"}),
            json!({"version": 1, "snapshot": "snapshot-.docx"}),
            json!({"version": 2, "snapshot": "snapshot-1.docx"}),
            json!({"snapshot": "snapshot-1.docx"}),
            json!([1, 2]),
        ] {
            write_atomic(&dir.join(MANIFEST), &serde_json::to_vec(&bad).unwrap()).unwrap();
            let mut app = app();
            assert_eq!(Recovery::new(dir.clone()).restore(&mut app), None, "{bad}");
        }
        fs::write(dir.join(MANIFEST), b"\xff\xfe not json").unwrap();
        assert_eq!(Recovery::new(dir.clone()).restore(&mut app()), None);
        // A manifest naming a snapshot that isn't a document.
        write_atomic(&dir.join(MANIFEST), &serde_json::to_vec(&json!({"version": 1, "snapshot": "snapshot-7.docx"})).unwrap()).unwrap();
        fs::write(dir.join("snapshot-7.docx"), b"not a zip").unwrap();
        let mut app = app();
        assert_eq!(Recovery::new(dir.clone()).restore(&mut app), None);
        assert!(!app.session.dirty);
        let _ = fs::remove_dir_all(dir);
    }
}
