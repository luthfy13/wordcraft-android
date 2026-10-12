//! Unsaved changes survive the app being killed or crashing: while the document has unsaved changes, a
//! copy of it is kept in the app's storage, and the next start opens that copy.
//!
//! Android kills apps in the background without warning, and a native crash ends the process, so the copy
//! is written as the user works: once edits pause for [`IDLE`], at least every [`EVERY`] while they don't,
//! and when the app leaves the screen. It's a Word document (macro-enabled, so nothing is left out; with the
//! document's password, if it has one), written on its own thread: the document is cloned (cheap, its
//! blocks are shared) and the main thread carries on.
//!
//! The folder holds `snapshot-<n>.docm` and `manifest.json`, which names the snapshot, the document's own
//! file and when the copy was taken. The manifest is written after its snapshot, so it never names a
//! snapshot that isn't complete. Saving the document, or closing it without saving, deletes both. A
//! password-protected snapshot whose password wasn't given is set aside (`held/`) and offered again at the
//! next start.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use wordcraft_doc::Document;
use wordcraft_engine::io::{ExportOptions, Password};
use wordcraft_ui_egui::WordApp;
use wordcraft_ui_egui::dialogs::Dialog;

use crate::store::write_atomic;

/// Edits pause this long before a snapshot.
pub const IDLE: Duration = Duration::from_secs(2);
/// Edits that don't pause are snapshotted this often.
pub const EVERY: Duration = Duration::from_secs(30);
/// A snapshot that couldn't be written is tried again after this.
const RETRY: Duration = Duration::from_secs(10);
const MANIFEST: &str = "manifest.json";
/// Where a snapshot waits whose password wasn't given.
const HELD: &str = "held";
/// A held snapshot is offered at this many starts at most.
const MAX_ASKS: u64 = 3;
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
    /// When the document was copied (ms since 1970), if known.
    captured: Option<u64>,
    /// Starts at which a held snapshot was offered.
    asked: u64,
}

impl Manifest {
    fn to_json(&self) -> Value {
        json!({
            "version": VERSION,
            "snapshot": self.snapshot,
            "path": self.path.as_ref().map(|p| p.to_string_lossy()),
            "encrypted": self.encrypted,
            "captured": self.captured,
            "asked": self.asked,
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
        Some(Self {
            snapshot,
            path,
            encrypted: v.get("encrypted").and_then(Value::as_bool).unwrap_or(false),
            captured: v.get("captured").and_then(Value::as_u64),
            asked: v.get("asked").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

fn is_snapshot_name(name: &str) -> bool {
    let n = name.strip_prefix("snapshot-").and_then(|n| n.strip_suffix(".docm").or_else(|| n.strip_suffix(".docx")));
    n.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn modified_ms(path: &Path) -> Option<u64> {
    let t = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    u64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// What a snapshot holds: which document, at which revision, with which password, saved where.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    document: u64,
    rev: u64,
    password: Option<Password>,
    path: Option<PathBuf>,
}

impl Key {
    fn of(app: &WordApp) -> Self {
        let s = &app.session;
        Self { document: s.document_id(), rev: s.rev(), password: s.password.clone(), path: s.path.clone() }
    }
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

/// The last job the writer finished, and the last one that failed.
#[derive(Default)]
struct Done {
    seq: Mutex<(u64, u64)>,
    cond: Condvar,
}

/// Keeps the snapshot of the open document up to date.
pub struct Recovery {
    dir: PathBuf,
    tx: Option<Sender<Job>>,
    done: Arc<Done>,
    seq: u64,
    /// What the newest snapshot holds (or will, once written), and its job.
    written: Option<(Key, u64)>,
    /// A snapshot (or its manifest) may be on disk.
    on_disk: bool,
    /// The state seen last, and when it was first seen.
    seen: Option<(Key, Instant)>,
    /// Since when the document has changes no snapshot holds.
    unsnapshotted: Option<Instant>,
    /// A snapshot failed to write: not again before this.
    retry_at: Option<Instant>,
    /// A password-protected snapshot being opened through the password dialog: the document it replaces,
    /// the manifest, the snapshot, and the folder it's in.
    restoring: Option<(u64, Manifest, PathBuf, PathBuf)>,
    /// What to tell the user once a password-protected snapshot opens.
    message: Option<String>,
}

impl Recovery {
    pub fn new(dir: PathBuf) -> Self {
        let on_disk = fs::read_dir(&dir).is_ok_and(|d| d.flatten().any(|e| e.file_name() != HELD));
        Self {
            dir,
            tx: None,
            done: Arc::default(),
            seq: 0,
            written: None,
            on_disk,
            seen: None,
            unsnapshotted: None,
            retry_at: None,
            restoring: None,
            message: None,
        }
    }

    fn held(&self) -> PathBuf {
        self.dir.join(HELD)
    }

    /// Opens the snapshot a previous run left, if there's one and the document wasn't saved after it (else
    /// one set aside earlier). Returns what to tell the user now.
    pub fn restore(&mut self, app: &mut WordApp) -> Option<String> {
        match read_manifest(&self.dir) {
            Some(manifest) => self.restore_from(app, self.dir.clone(), manifest),
            None => {
                // Leftovers of a write that was killed.
                self.clear_now();
                let held = self.held();
                let manifest = read_manifest(&held)?;
                self.restore_from(app, held, manifest)
            }
        }
    }

    fn restore_from(&mut self, app: &mut WordApp, dir: PathBuf, mut manifest: Manifest) -> Option<String> {
        let snapshot = dir.join(&manifest.snapshot);
        let Some(snapshot_time) = modified_ms(&snapshot) else {
            remove_dir_contents(&dir);
            return None;
        };
        // The document's file saved after the copy was taken holds the newer text.
        let captured = manifest.captured.unwrap_or(snapshot_time);
        if let Some(path) = &manifest.path
            && modified_ms(path).is_some_and(|t| t > captured)
        {
            log::info!("the document was saved after its recovery snapshot: discarding the snapshot");
            remove_dir_contents(&dir);
            return None;
        }
        let name = manifest.path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
        if manifest.encrypted {
            if manifest.asked >= MAX_ASKS {
                log::info!("a password-protected recovery snapshot was declined {MAX_ASKS} times: discarding it");
                remove_dir_contents(&dir);
                return None;
            }
            // The password dialog opens it; `observe` finishes the job.
            let before = app.session.document_id();
            let shown = name.clone().unwrap_or_else(|| "The unsaved document".to_owned());
            app.dialog = Some(Dialog::Password {
                name: format!("{shown} (unsaved changes recovered)"),
                params: json!({"path": snapshot.to_string_lossy()}),
                password: Password::default(),
                message: String::new(),
            });
            manifest.asked = manifest.asked.saturating_add(1);
            self.message = Some(recovered_message(name.as_deref()));
            self.restoring = Some((before, manifest, snapshot, dir));
            return None;
        }
        match wordcraft_engine::io::open_path(&snapshot) {
            Ok(doc) => {
                app.session.set_document(doc);
                app.session.path = manifest.path;
                app.session.touch();
                // The snapshot stays until the next one or a save (the folder's own; a held one is copied in).
                if dir != self.dir {
                    self.adopt(&dir, &manifest.snapshot);
                }
                self.written = Some((Key::of(app), 0));
                Some(recovered_message(name.as_deref()))
            }
            Err(e) => {
                log::error!("can't open the recovery snapshot: {e}");
                remove_dir_contents(&dir);
                None
            }
        }
    }

    /// A held snapshot that opened becomes the current one.
    fn adopt(&mut self, held: &Path, snapshot: &str) {
        let _ = fs::create_dir_all(&self.dir);
        for name in [snapshot, MANIFEST] {
            if let Err(e) = fs::rename(held.join(name), self.dir.join(name)) {
                log::warn!("can't move the recovered snapshot: {e}");
            }
        }
        let _ = fs::remove_dir_all(held);
        self.on_disk = true;
    }

    /// After each frame and each wake-up: writes a snapshot when one is due, deletes it once the document
    /// is saved. Returns when to look again.
    pub fn observe(&mut self, app: &mut WordApp, now: Instant) -> Option<Instant> {
        self.finish_restoring(app);
        self.check_failed(now);
        let key = Key::of(app);
        if !app.session.dirty {
            self.seen = None;
            self.unsnapshotted = None;
            if self.on_disk && self.restoring.is_none() {
                self.clear();
            }
            return None;
        }
        if self.written.as_ref().is_some_and(|(k, _)| *k == key) {
            self.unsnapshotted = None;
            return None;
        }
        let changed = match &self.seen {
            Some((k, at)) if *k == key => *at,
            _ => {
                self.seen = Some((key, now));
                now
            }
        };
        let since = *self.unsnapshotted.get_or_insert(now);
        let due = changed.checked_add(IDLE).into_iter().chain(since.checked_add(EVERY)).min()?;
        let due = self.retry_at.map_or(due, |r| due.max(r));
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
        self.check_failed(Instant::now());
        if app.session.dirty && !self.written.as_ref().is_some_and(|(k, _)| *k == Key::of(app)) {
            self.write(app);
        }
    }

    /// The user closed or quit, saving or not: nothing to recover (a held snapshot is kept: its document
    /// never opened).
    pub fn discard(&mut self) {
        self.restoring = None;
        self.clear();
    }

    /// Waits up to `timeout` for the writer to finish; whether it did.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut done = self.done.seq.lock().unwrap_or_else(PoisonError::into_inner);
        while done.0 < self.seq {
            let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return false;
            }
            done = self.done.cond.wait_timeout(done, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }

    /// The newest snapshot failed to write: it's written again (not at once).
    fn check_failed(&mut self, now: Instant) {
        let failed = self.done.seq.lock().unwrap_or_else(PoisonError::into_inner).1;
        if let Some((_, seq)) = &self.written
            && *seq != 0
            && *seq == failed
        {
            log::warn!("the recovery snapshot wasn't written: trying again");
            self.written = None;
            self.retry_at = now.checked_add(RETRY);
        }
    }

    /// A password-protected snapshot opened through the password dialog gets its document's file back. One
    /// that didn't open (cancelled, or another document opened first) is set aside for the next start.
    fn finish_restoring(&mut self, app: &mut WordApp) {
        let Some((before, _, snapshot, _)) = &self.restoring else { return };
        let snapshot_path = snapshot.to_string_lossy().into_owned();
        let asking = |app: &WordApp| matches!(&app.dialog, Some(Dialog::Password { params, .. }) if params.get("path").and_then(Value::as_str) == Some(snapshot_path.as_str()));
        if app.session.document_id() == *before && asking(app) {
            return;
        }
        // Another document opened while asking: the question goes with it.
        if asking(app) {
            app.dialog = None;
        }
        let opened = app.session.document_id() != *before && app.session.path.as_deref() == Some(snapshot.as_path());
        let Some((_, manifest, snapshot, dir)) = self.restoring.take() else { return };
        let snapshot_text = snapshot.to_string_lossy().into_owned();
        app.ui.recent.retain(|r| *r != snapshot_text);
        if opened {
            app.session.path = manifest.path.clone();
            app.session.touch();
            if dir != self.dir {
                self.adopt(&dir, &manifest.snapshot);
            }
            if let Some(m) = self.message.take() {
                log::info!("{m}");
                app.status(m);
            }
            return;
        }
        self.message = None;
        log::info!("the recovered document wasn't opened: keeping its snapshot for the next start");
        let held = self.held();
        let moved = (|| -> std::io::Result<()> {
            if dir != held {
                let _ = fs::remove_dir_all(&held);
                fs::create_dir_all(&held)?;
                fs::rename(&snapshot, held.join(&manifest.snapshot))?;
            }
            let bytes = serde_json::to_vec(&manifest.to_json()).map_err(std::io::Error::other)?;
            write_atomic(&held.join(MANIFEST), &bytes)?;
            if dir != held {
                let _ = fs::remove_file(dir.join(MANIFEST));
            }
            Ok(())
        })();
        if let Err(e) = moved {
            log::error!("can't set the recovered snapshot aside: {e}");
        }
    }

    fn write(&mut self, app: &WordApp) {
        let s = &app.session;
        self.seq = self.seq.wrapping_add(1).max(1);
        let manifest = Manifest {
            snapshot: format!("snapshot-{}.docm", self.seq),
            path: s.path.clone(),
            encrypted: s.password.is_some(),
            captured: Some(now_ms()),
            asked: 0,
        };
        let job = Job::Write { seq: self.seq, doc: Box::new(s.doc.clone()), password: s.password.clone(), manifest };
        self.written = Some((Key::of(app), self.seq));
        self.unsnapshotted = None;
        self.retry_at = None;
        self.on_disk = true;
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
        let ok = run(&self.dir, job);
        mark_done(&self.done, seq, ok);
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

fn read_manifest(dir: &Path) -> Option<Manifest> {
    let path = dir.join(MANIFEST);
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
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(dir, job))).unwrap_or_else(|_| {
            log::error!("writing the recovery snapshot panicked");
            false
        });
        mark_done(done, seq, ok);
    }
}

fn mark_done(done: &Done, seq: u64, ok: bool) {
    let mut d = done.seq.lock().unwrap_or_else(PoisonError::into_inner);
    d.0 = d.0.max(seq);
    if !ok {
        d.1 = seq;
    }
    done.cond.notify_all();
}

/// Whether the job succeeded.
fn run(dir: &Path, job: Job) -> bool {
    match job {
        Job::Write { doc, password, manifest, .. } => {
            let started = Instant::now();
            if let Err(e) = write_snapshot(dir, &doc, password.as_ref(), &manifest) {
                log::error!("can't write the recovery snapshot: {e}");
                return false;
            }
            log::info!("recovery snapshot written in {} ms", started.elapsed().as_millis());
            remove_snapshots(dir, Some(&manifest.snapshot));
            true
        }
        Job::Clear { .. } => {
            clear_dir(dir);
            true
        }
    }
}

fn write_snapshot(dir: &Path, doc: &Document, password: Option<&Password>, manifest: &Manifest) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let snapshot = dir.join(&manifest.snapshot);
    wordcraft_engine::io::save_path_with(&snapshot, doc, password.map(Password::as_str), &ExportOptions::default())?;
    let bytes = serde_json::to_vec(&manifest.to_json()).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(MANIFEST), &bytes).map_err(|e| e.to_string())
}

/// The manifest first, so no manifest names a missing snapshot. A held snapshot stays.
fn clear_dir(dir: &Path) {
    match fs::remove_file(dir.join(MANIFEST)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("can't delete the recovery manifest: {e}"),
    }
    remove_snapshots(dir, None);
}

/// Everything in a folder (a snapshot not worth keeping).
fn remove_dir_contents(dir: &Path) {
    clear_dir(dir);
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
        assert_eq!(files(&dir), ["manifest.json", "snapshot-1.docm"]);
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
        let snapshot = dir.join("recovery").join("snapshot-1.docm");
        assert!(wordcraft_engine::io::open_path(&snapshot).is_err(), "the snapshot is encrypted");

        let mut second = app();
        let mut rec = Recovery::new(dir.join("recovery"));
        assert_eq!(rec.restore(&mut second), None, "nothing to say until it opens");
        let Some(Dialog::Password { name, .. }) = &second.dialog else { panic!("the password dialog is open") };
        assert!(name.starts_with("secret.docx"), "{name}");
        assert_eq!(rec.observe(&mut second, Instant::now()), None);
        // The user types the password (what the dialog's OK runs).
        second.dialog = None;
        second.execute("file.open", json!({"path": snapshot.to_string_lossy(), "password": "pw"})).unwrap();
        rec.observe(&mut second, Instant::now());
        assert!(body(&second).contains("top secret"));
        assert_eq!(second.session.path.as_deref(), Some(doc_path.as_path()));
        assert!(second.session.dirty);
        assert!(!second.ui.recent.iter().any(|r| r.contains("snapshot")), "the snapshot isn't a recent document");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cancelled_password_keeps_the_snapshot_for_the_next_start() {
        let dir = temp_dir("rec-held");
        let mut first = app();
        type_text(&mut first, "kept for later");
        first.session.password = Some(Password::new("pw"));
        let mut rec = Recovery::new(dir.clone());
        rec.flush(&mut first);
        drop(rec);

        let mut second = app();
        let mut rec = Recovery::new(dir.clone());
        rec.restore(&mut second);
        // Cancelled (or Back), then another document is typed in and saved: the held copy stays.
        second.dialog = None;
        rec.observe(&mut second, Instant::now());
        type_text(&mut second, "something else");
        rec.flush(&mut second);
        rec.discard();
        assert!(rec.wait(Duration::from_secs(10)));
        assert!(dir.join(HELD).join(MANIFEST).is_file());
        drop(rec);

        let mut third = app();
        let mut rec = Recovery::new(dir.clone());
        rec.restore(&mut third);
        let Some(Dialog::Password { params, .. }) = &third.dialog else { panic!("asked again") };
        let snapshot = params["path"].as_str().unwrap().to_owned();
        third.dialog = None;
        third.execute("file.open", json!({"path": snapshot, "password": "pw"})).unwrap();
        rec.observe(&mut third, Instant::now());
        assert!(body(&third).contains("kept for later"));
        assert!(!dir.join(HELD).exists(), "taken back into the recovery folder");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_password_is_snapshotted() {
        let dir = temp_dir("rec-new-password");
        let mut app = app();
        type_text(&mut app, "text");
        let mut rec = Recovery::new(dir.clone());
        rec.flush(&mut app);
        assert!(rec.wait(Duration::from_secs(10)));
        // Protect Document changes the password, not the text.
        app.run("file.encrypt", json!({"password": "new"})).unwrap();
        rec.flush(&mut app);
        assert!(rec.wait(Duration::from_secs(10)));
        assert_eq!(files(&dir), ["manifest.json", "snapshot-2.docm"]);
        assert!(wordcraft_engine::io::open_path(&dir.join("snapshot-2.docm")).is_err(), "written with the password");
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
