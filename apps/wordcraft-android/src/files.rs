//! Documents in Android's storage (the Storage Access Framework), behind the app's file dialogs.
//!
//! The app reads and writes files by path; Android hands out documents as `content://` URIs. So each
//! document gets a **mirror**: a copy in the app's storage (`files/wordcraft/docs/<n>/<name>`) that the
//! app opens, saves and AutoSaves as on the desktop. The host copies the mirror back to the document
//! whenever it changes (a *write-back*), and remembers the document's URI, with the permission to use it
//! again, for Recent.
//!
//! - **Open** (the `document` purpose): the system picker, the document copied in, the dialog answered
//!   with the mirror. A mirror holding changes not yet written back is opened as it is.
//! - **Save As, Export, Save as PDF**: the picker creates the document, the dialog is answered with an
//!   empty mirror, and the write the app then makes is written back.
//! - **Pictures, fonts, recipient lists, templates**: copied in for the app to read once.
//! - **Open with, Share**: the document Android sent is copied in and opened as a dropped file is.
//!
//! A save is noticed by the mirror's [`Stamp`] changing (the app replaces a file it saves), checked
//! after each frame for the open document and for files a Save dialog handed out. A write-back that
//! fails leaves the document marked unsaved; one that finds the document changed elsewhere asks first.
//!
//! This module decides; the platform does the I/O ([`FileOp`] out, [`FileEvent`] back).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use serde_json::{Value, json};
use wordcraft_ui_egui::WordApp;
use wordcraft_ui_egui::file_dialogs::{FileDialogAnswer, FileDialogRequest, open_filters};

use crate::prompts::Exported;
use crate::store::write_atomic;

/// Documents remembered at most (Recent lists 50); more only while they hold changes not written back.
const MAX_ENTRIES: usize = 64;
/// Entries read from `docs.json` at most (a damaged or hostile file).
const MAX_LOADED: usize = 1024;
/// Longest mirror file name kept, in characters.
const MAX_NAME: usize = 120;
/// Largest `docs.json` read.
const MAX_REGISTRY: u64 = 4 << 20;
const REGISTRY_VERSION: u64 = 1;

/// A file's identity and content version: a save replaces the file (a new inode) or rewrites it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    modified: i128,
    inode: u64,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Self> {
        let m = fs::metadata(path).ok()?;
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128);
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self { len: m.len(), modified, inode })
    }
}

/// What the document provider reported for a document: its last change (ms since 1970) and size, each
/// when known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Remote {
    pub modified: Option<i64>,
    pub size: Option<i64>,
}

impl Remote {
    fn to_json(self) -> Value {
        json!({"modified": self.modified, "size": self.size})
    }

    fn from_json(v: Option<&Value>) -> Self {
        let v = v.unwrap_or(&Value::Null);
        Self { modified: v.get("modified").and_then(Value::as_i64), size: v.get("size").and_then(Value::as_i64) }
    }
}

/// A document the user picked, as the platform reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub uri: String,
    /// The provider's display name.
    pub name: String,
    pub mime: Option<String>,
    /// The provider accepts writes, and the app was allowed to write.
    pub writable: bool,
    /// The permission outlives the app (Recent works after a restart).
    pub persisted: bool,
    /// A virtual document (no bytes to copy).
    pub is_virtual: bool,
    pub remote: Remote,
}

/// Why a write-back failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The document changed elsewhere since it was copied in or last written: this is what it is now.
    Conflict(Remote),
    /// No permission any more, or the document is gone.
    Lost(String),
    Failed(String),
}

/// What the platform is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    /// Show the system picker. `create`: a new document with this name and type; otherwise open one of
    /// `mimes`. `persist`: keep the permission (for documents, which Recent opens again).
    Pick { req: u64, create: Option<(String, String)>, mimes: Vec<String>, persist: bool },
    /// Copy the document at `uri` to `path`, replacing it.
    CopyIn { req: u64, uri: String, path: PathBuf },
    /// Copy `path` to the document at `uri`, replacing its content. Unless `force`, first check it still
    /// is what `expect` says.
    WriteBack { entry: u64, uri: String, path: PathBuf, expect: Remote, force: bool },
    /// Check the document at `uri` against `expect`; if it changed, copy it to `path`.
    Refresh { req: u64, uri: String, path: PathBuf, expect: Remote },
    /// Report which permissions are still held ([`FileEvent::Grants`]).
    ListGrants,
    /// Give up the permission for `uri`.
    Release { uri: String },
}

/// What the platform reports back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEvent {
    /// The picker closed: the document, `None` when cancelled, or why it couldn't be shown.
    Picked {
        req: u64,
        result: Result<Option<Picked>, String>,
    },
    CopiedIn {
        req: u64,
        result: Result<Remote, String>,
    },
    WroteBack {
        entry: u64,
        result: Result<Remote, WriteError>,
    },
    /// `Some`: the document had changed and was copied.
    Refreshed {
        req: u64,
        result: Result<Option<Remote>, String>,
    },
    /// Android sent a document (Open with, Share). `req` names the copy-in to follow.
    Incoming {
        picked: Picked,
    },
    /// The permissions held.
    Grants {
        uris: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sync {
    /// The document holds what the mirror holds.
    Clean,
    /// The mirror changed and isn't written back yet (or is being written).
    Pending,
    /// Writing back failed: the changes are only in the mirror.
    Failed,
}

/// A remembered document.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    id: u64,
    uri: String,
    /// The mirror, relative to the documents folder (`<id>/<name>`).
    file: PathBuf,
    writable: bool,
    persisted: bool,
    remote: Remote,
    sync: Sync,
    /// The mirror as last seen (not kept between runs).
    stamp: Option<Stamp>,
    /// A write-back is under way.
    in_flight: bool,
    /// The mirror changed again while a write-back was under way.
    again: bool,
    /// Handed out by a Save dialog and not written yet: watched after each frame.
    watch: bool,
    /// Written by an export (not the open document): offered for sharing once written back.
    export: bool,
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "uri": self.uri,
            "file": self.file.to_string_lossy(),
            "writable": self.writable,
            "persisted": self.persisted,
            "remote": self.remote.to_json(),
            "pending": self.sync != Sync::Clean,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let file = PathBuf::from(v.get("file")?.as_str()?);
        // Only `<id>/<name>` inside the documents folder.
        let mut parts = file.components();
        let ok = matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), Some(std::path::Component::Normal(_)), None)
        );
        if !ok {
            return None;
        }
        Some(Self {
            id: v.get("id")?.as_u64()?,
            uri: v.get("uri")?.as_str()?.to_owned(),
            file,
            writable: v.get("writable").and_then(Value::as_bool).unwrap_or(false),
            persisted: v.get("persisted").and_then(Value::as_bool).unwrap_or(false),
            remote: Remote::from_json(v.get("remote")),
            sync: if v.get("pending").and_then(Value::as_bool).unwrap_or(false) { Sync::Pending } else { Sync::Clean },
            stamp: None,
            in_flight: false,
            again: false,
            watch: false,
            export: false,
        })
    }

    fn display_name(&self) -> String {
        self.file.file_name().map_or_else(|| "the document".to_owned(), |n| n.to_string_lossy().into_owned())
    }
}

/// The file dialog the app is waiting on.
struct Pending {
    req: u64,
    request: FileDialogRequest,
    tx: Sender<FileDialogAnswer>,
    /// The picked document is being copied in (`Then::Answer`).
    copying: bool,
}

/// What a finished copy leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Answer the open file dialog with the document.
    Answer,
    /// Open it as a dropped file (Open with, Share).
    Open,
    /// Open it again if it's the open, untouched document (a newer version was found).
    Reopen,
}

/// A copy from Android's storage under way.
struct Copy {
    req: u64,
    /// The document's entry; `None` for a picture, font or the like, read once.
    entry: Option<u64>,
    /// Where the platform writes.
    dest: PathBuf,
    /// For a document already here: its mirror, and the mirror's stamp when the copy began. The copy goes
    /// beside it and replaces it only if nothing changed the mirror meanwhile.
    replaces: Option<(PathBuf, Option<Stamp>)>,
    then: Then,
}

/// Shared with the app's `file_dialog` hook.
#[derive(Default)]
pub struct Dialogs {
    pending: Option<Pending>,
    asked: Vec<(FileDialogRequest, Sender<FileDialogAnswer>)>,
}

impl Dialogs {
    /// The hook: queues the request; [`Files::after_frame`] shows the picker.
    pub fn ask(this: &Rc<RefCell<Self>>, request: FileDialogRequest) -> Receiver<FileDialogAnswer> {
        let (tx, rx) = std::sync::mpsc::channel();
        this.borrow_mut().asked.push((request, tx));
        rx
    }
}

/// Something for the user to decide, shown by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Question {
    /// The document changed elsewhere: replace it, save a copy, or keep the changes here for now.
    Conflict { entry: u64, name: String },
}

/// The user's answer to a [`Question::Conflict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Replace,
    SaveCopy,
    Keep,
}

/// The documents and the picker flow.
pub struct Files {
    /// `files/wordcraft/docs`.
    docs: PathBuf,
    /// `files/wordcraft/imports`, emptied at start.
    imports: PathBuf,
    entries: Vec<Entry>,
    next: u64,
    dialogs: Rc<RefCell<Dialogs>>,
    ops: Vec<FileOp>,
    copies: Vec<Copy>,
    /// Files to open as dropped files, next frame.
    to_open: Vec<PathBuf>,
    questions: Vec<Question>,
    notices: Vec<String>,
    exports: Vec<Exported>,
    /// The document seen after the last frame.
    document: Option<(u64, Option<PathBuf>)>,
    /// The registry changed since it was written.
    changed: bool,
}

impl Files {
    /// The documents remembered in `root` (`files/wordcraft`).
    pub fn new(root: &Path) -> Self {
        let docs = root.join("docs");
        let imports = root.join("imports");
        let _ = fs::remove_dir_all(&imports);
        let mut files = Self {
            docs,
            imports,
            entries: Vec::new(),
            next: 1,
            dialogs: Rc::default(),
            ops: Vec::new(),
            copies: Vec::new(),
            to_open: Vec::new(),
            questions: Vec::new(),
            notices: Vec::new(),
            exports: Vec::new(),
            document: None,
            changed: false,
        };
        files.load();
        files.remove_leftovers();
        files.ops.push(FileOp::ListGrants);
        // Changes a previous run couldn't write back: try again.
        let pending: Vec<u64> = files.entries.iter().filter(|e| e.sync != Sync::Clean && e.writable).map(|e| e.id).collect();
        for id in pending {
            files.write_back(id, false);
        }
        files
    }

    /// The state the `file_dialog` hook shares.
    pub fn dialogs(&self) -> Rc<RefCell<Dialogs>> {
        Rc::clone(&self.dialogs)
    }

    fn registry(&self) -> PathBuf {
        self.docs.join("docs.json")
    }

    fn load(&mut self) {
        let path = self.registry();
        let bytes = match fs::metadata(&path) {
            Ok(m) if m.len() <= MAX_REGISTRY => fs::read(&path).ok(),
            _ => None,
        };
        let Some(v) = bytes.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
        if v.get("version").and_then(Value::as_u64) != Some(REGISTRY_VERSION) {
            return;
        }
        let mut seen = BTreeSet::new();
        for e in v.get("documents").and_then(Value::as_array).into_iter().flatten().filter_map(Entry::from_json) {
            let mirror = self.docs.join(&e.file);
            if seen.insert(e.id) && self.entries.len() < MAX_LOADED && mirror.is_file() {
                self.next = self.next.max(e.id.saturating_add(1));
                self.entries.push(Entry { stamp: Stamp::of(&mirror), ..e });
            }
        }
        self.next = self.next.max(v.get("next").and_then(Value::as_u64).unwrap_or(1));
    }

    /// Copies a killed process left beside the mirrors, and temporary files.
    fn remove_leftovers(&self) {
        let dirs = std::iter::once(self.docs.clone()).chain(self.entries.iter().filter_map(|e| self.mirror(e).parent().map(Path::to_path_buf)));
        for dir in dirs {
            let Ok(list) = fs::read_dir(&dir) else { continue };
            for f in list.flatten() {
                let name = f.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') && (name.ends_with(".incoming") || name.ends_with(".copy") || name.ends_with(".tmp")) {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
    }

    /// Writes `docs.json` if it changed.
    pub fn save(&mut self) {
        if !self.changed {
            return;
        }
        let v = json!({"version": REGISTRY_VERSION, "next": self.next, "documents": self.entries.iter().map(Entry::to_json).collect::<Vec<_>>()});
        match serde_json::to_vec(&v).map_err(std::io::Error::other).and_then(|b| write_atomic(&self.registry(), &b)) {
            Ok(()) => self.changed = false,
            Err(e) => log::error!("can't save the document list: {e}"),
        }
    }

    fn id(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        self.changed = true;
        id
    }

    fn mirror(&self, e: &Entry) -> PathBuf {
        self.docs.join(&e.file)
    }

    fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.changed = true;
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// The entry whose mirror is `path`.
    fn entry_at(&self, path: &Path) -> Option<&Entry> {
        let rel = path.strip_prefix(&self.docs).ok()?;
        self.entries.iter().find(|e| e.file == rel)
    }

    /// The entry for `uri`, made if new, with a mirror named after `name`.
    fn entry_for(&mut self, picked: &Picked, name: &str) -> u64 {
        if let Some(i) = self.entries.iter().position(|e| e.uri == picked.uri) {
            let mut e = self.entries.remove(i);
            e.writable = picked.writable;
            e.persisted = e.persisted || picked.persisted;
            let id = e.id;
            // Most recent first.
            self.entries.insert(0, e);
            self.changed = true;
            return id;
        }
        let id = self.id();
        let file = PathBuf::from(id.to_string()).join(name);
        self.entries.insert(
            0,
            Entry {
                id,
                uri: picked.uri.clone(),
                file,
                writable: picked.writable,
                persisted: picked.persisted,
                remote: picked.remote,
                sync: Sync::Clean,
                stamp: None,
                in_flight: false,
                again: false,
                watch: false,
                export: false,
            },
        );
        id
    }

    /// The platform's report.
    pub fn event(&mut self, event: FileEvent, app: &mut WordApp) {
        match event {
            FileEvent::Picked { req, result } => self.picked(req, result),
            FileEvent::CopiedIn { req, result } => self.copied_in(req, result, app),
            FileEvent::WroteBack { entry, result } => self.wrote_back(entry, result, app),
            FileEvent::Refreshed { req, result } => self.finish_copy(req, result, app),
            FileEvent::Incoming { picked } => self.incoming(picked),
            FileEvent::Grants { uris } => self.grants(&uris, app),
        }
    }

    fn picked(&mut self, req: u64, result: Result<Option<Picked>, String>) {
        let mut dialogs = self.dialogs.borrow_mut();
        let Some(p) = dialogs.pending.take_if(|p| p.req == req && !p.copying) else {
            log::warn!("a picker answered a request that's gone");
            return;
        };
        drop(dialogs);
        let picked = match result {
            Ok(Some(picked)) => picked,
            Ok(None) => return answer(p.tx, Ok(None)),
            // The app's own file browser instead.
            Err(e) => return answer(p.tx, Err(e)),
        };
        if picked.is_virtual {
            self.notices.push(format!("{} can't be opened: it has no file to read (a virtual document)", picked.name));
            return answer(p.tx, Ok(None));
        }
        match &p.request {
            FileDialogRequest::Save { name } => {
                let file = save_name(&picked.name, name);
                let id = self.entry_for(&picked, &file);
                let Some(e) = self.entry(id) else { return answer(p.tx, Ok(None)) };
                let mirror = self.mirror(e);
                if let Err(err) = mirror.parent().map_or(Ok(()), fs::create_dir_all) {
                    self.notices.push(format!("Can't save {file}: {err}"));
                    return answer(p.tx, Ok(None));
                }
                // A document made for this save is empty, so a first write is a change.
                let stamp = Stamp::of(&mirror);
                for e in &mut self.entries {
                    e.watch = false;
                }
                if let Some(e) = self.entry_mut(id) {
                    e.watch = true;
                    e.stamp = stamp;
                    e.writable = true;
                    e.remote = picked.remote;
                }
                answer(p.tx, Ok(Some(mirror.to_string_lossy().into_owned())));
            }
            FileDialogRequest::Open { purpose } => {
                let name = open_name(&picked.name, picked.mime.as_deref());
                let copy = if purpose == "document" {
                    let id = self.entry_for(&picked, &name);
                    // Changes not written back yet are newer than the document: they open as they are.
                    if let Some(mirror) = self.unsynced_mirror(id) {
                        return answer(p.tx, Ok(Some(mirror.to_string_lossy().into_owned())));
                    }
                    self.document_copy(req, id, Then::Answer)
                } else {
                    let n = self.id();
                    let dest = self.imports.join(n.to_string()).join(&name);
                    Some(Copy { req, entry: None, dest, replaces: None, then: Then::Answer })
                };
                let Some(copy) = copy else { return answer(p.tx, Ok(None)) };
                if let Some(dir) = copy.dest.parent()
                    && let Err(err) = fs::create_dir_all(dir)
                {
                    self.notices.push(format!("Can't open {name}: {err}"));
                    return answer(p.tx, Ok(None));
                }
                self.ops.push(FileOp::CopyIn { req, uri: picked.uri.clone(), path: copy.dest.clone() });
                self.copies.push(copy);
                self.dialogs.borrow_mut().pending = Some(Pending { copying: true, ..p });
            }
        }
    }

    /// The mirror of entry `id` if it holds changes not written back (or being written), which are newer
    /// than the document: it's opened as it is, with a word.
    fn unsynced_mirror(&mut self, id: u64) -> Option<PathBuf> {
        let e = self.entry(id)?;
        let mirror = self.mirror(e);
        if (e.sync == Sync::Clean && !e.in_flight) || !mirror.is_file() {
            return None;
        }
        self.notices.push(format!("{} has changes that aren't saved to its file yet: opening them", e.display_name()));
        Some(mirror)
    }

    /// A copy of entry `id`'s document: into its mirror if there's none yet, else beside it.
    fn document_copy(&self, req: u64, id: u64, then: Then) -> Option<Copy> {
        let e = self.entry(id)?;
        let mirror = self.mirror(e);
        if !mirror.is_file() {
            return Some(Copy { req, entry: Some(id), dest: mirror, replaces: None, then });
        }
        let name = mirror.file_name()?.to_string_lossy().into_owned();
        let dest = mirror.with_file_name(format!(".{name}.incoming"));
        let stamp = Stamp::of(&mirror);
        Some(Copy { req, entry: Some(id), dest, replaces: Some((mirror, stamp)), then })
    }

    fn copied_in(&mut self, req: u64, result: Result<Remote, String>, app: &mut WordApp) {
        self.finish_copy(req, result.map(Some), app);
    }

    /// A copy finished (`Ok(None)`: a refresh found nothing new). A copy beside a mirror replaces it only if
    /// the mirror is as it was, holds no changes still to write back, and isn't open with unsaved changes:
    /// otherwise the changes here stay, and the next write-back finds the conflict.
    fn finish_copy(&mut self, req: u64, result: Result<Option<Remote>, String>, app: &mut WordApp) {
        let Some(i) = self.copies.iter().position(|c| c.req == req) else { return };
        let copy = self.copies.remove(i);
        let path = copy.replaces.as_ref().map_or_else(|| copy.dest.clone(), |(mirror, _)| mirror.clone());
        let fresh = match result {
            Ok(Some(remote)) => match (copy.entry, &copy.replaces) {
                (Some(id), Some((mirror, stamp))) => {
                    let edited = app.session.path.as_deref() == Some(mirror.as_path()) && app.session.dirty;
                    let untouched = self.entry(id).is_some_and(|e| e.sync == Sync::Clean && !e.in_flight) && Stamp::of(mirror) == *stamp;
                    if untouched && !edited && fs::rename(&copy.dest, mirror).is_ok() {
                        self.synced_in(id, remote, mirror);
                        true
                    } else {
                        let _ = fs::remove_file(&copy.dest);
                        log::info!("a newer version of {} wasn't copied over the changes made here", mirror.display());
                        false
                    }
                }
                (Some(id), None) => {
                    self.synced_in(id, remote, &copy.dest);
                    true
                }
                (None, _) => true,
            },
            Ok(None) => {
                if copy.replaces.is_some() {
                    let _ = fs::remove_file(&copy.dest);
                }
                false
            }
            Err(e) => {
                if copy.replaces.is_some() {
                    let _ = fs::remove_file(&copy.dest);
                }
                // A document read before opens as it was kept here (offline, say).
                let kept = copy.replaces.is_some() && copy.then != Then::Reopen;
                if copy.then == Then::Reopen {
                    log::info!("can't check {} for changes: {e}", path.display());
                } else if kept {
                    self.notices.push(format!("Can't read the document now ({e}): opening the copy kept on this device"));
                } else {
                    self.notices.push(format!("Can't open the document: {e}"));
                }
                if !kept {
                    if copy.then == Then::Answer {
                        self.answer_copy(Ok(None));
                    }
                    return;
                }
                false
            }
        };
        match copy.then {
            Then::Answer => self.answer_copy(Ok(Some(path.to_string_lossy().into_owned()))),
            Then::Open => {
                log::info!("opening a document sent to the app");
                self.to_open.push(path);
            }
            Then::Reopen => {
                if fresh && app.session.path.as_deref() == Some(path.as_path()) && !app.session.dirty {
                    log::info!("{} changed elsewhere: opening the new version", path.display());
                    let _ = app.run("file.open", json!({"path": path.to_string_lossy()}));
                }
            }
        }
    }

    /// Answers the dialog waiting on a copy.
    fn answer_copy(&mut self, result: FileDialogAnswer) {
        let pending = self.dialogs.borrow_mut().pending.take_if(|p| p.copying);
        if let Some(p) = pending {
            answer(p.tx, result);
        }
    }

    fn synced_in(&mut self, id: u64, remote: Remote, mirror: &Path) {
        let stamp = Stamp::of(mirror);
        if let Some(e) = self.entry_mut(id) {
            e.remote = remote;
            e.stamp = stamp;
            e.sync = Sync::Clean;
        }
    }

    fn incoming(&mut self, picked: Picked) {
        if picked.is_virtual {
            self.notices.push(format!("{} can't be opened: it has no file to read", picked.name));
            return;
        }
        let name = open_name(&picked.name, picked.mime.as_deref());
        let req = self.id();
        let copy = if is_document_name(&name) {
            let id = self.entry_for(&picked, &name);
            if let Some(mirror) = self.unsynced_mirror(id) {
                self.to_open.push(mirror);
                return;
            }
            self.document_copy(req, id, Then::Open)
        } else {
            let dest = self.imports.join(req.to_string()).join(&name);
            Some(Copy { req, entry: None, dest, replaces: None, then: Then::Open })
        };
        let Some(copy) = copy else { return };
        if let Some(dir) = copy.dest.parent() {
            let _ = fs::create_dir_all(dir);
        }
        self.ops.push(FileOp::CopyIn { req, uri: picked.uri, path: copy.dest.clone() });
        self.copies.push(copy);
    }

    fn grants(&mut self, uris: &[String], app: &mut WordApp) {
        let held: BTreeSet<&str> = uris.iter().map(String::as_str).collect();
        for e in &mut self.entries {
            if e.persisted && !held.contains(e.uri.as_str()) {
                log::info!("the permission for {} is gone: it opens read-only", e.display_name());
                e.persisted = false;
                e.writable = false;
                self.changed = true;
            }
        }
        self.collect_garbage(app);
    }

    /// Forgets documents Recent no longer lists (their mirrors and permissions), keeping the open one and
    /// any with changes not written back; drops Recent's mirrors that are gone.
    pub fn collect_garbage(&mut self, app: &mut WordApp) {
        let docs = self.docs.clone();
        let known: BTreeSet<PathBuf> = self.entries.iter().map(|e| docs.join(&e.file)).collect();
        app.ui.recent.retain(|r| {
            let p = Path::new(r);
            !p.starts_with(&docs) || (known.contains(p) && p.is_file())
        });
        let keep: BTreeSet<PathBuf> = app.ui.recent.iter().map(PathBuf::from).chain(app.session.path.clone()).collect();
        // Documents being copied in.
        let copying: BTreeSet<u64> = self.copies.iter().filter_map(|c| c.entry).collect();
        let mut gone = Vec::new();
        self.entries.retain(|e| {
            let mirror = docs.join(&e.file);
            let kept = keep.contains(&mirror) || e.sync != Sync::Clean || e.in_flight || e.watch || copying.contains(&e.id);
            if !kept {
                gone.push(e.clone());
            }
            kept
        });
        // Over the cap: the oldest clean ones go, even if listed, but not the open one or one in use.
        let open = app.session.path.clone();
        while self.entries.len() > MAX_ENTRIES {
            let spare = |e: &Entry| {
                e.sync == Sync::Clean && !e.in_flight && !e.watch && !copying.contains(&e.id) && open.as_deref() != Some(docs.join(&e.file).as_path())
            };
            match self.entries.iter().rposition(spare) {
                Some(i) => gone.push(self.entries.remove(i)),
                None => break,
            }
        }
        for e in gone {
            self.changed = true;
            if let Some(dir) = docs.join(&e.file).parent() {
                let _ = fs::remove_dir_all(dir);
            }
            if e.persisted {
                self.ops.push(FileOp::Release { uri: e.uri });
            }
        }
    }

    fn write_back(&mut self, id: u64, force: bool) {
        let Some(e) = self.entry(id) else { return };
        let op = FileOp::WriteBack { entry: id, uri: e.uri.clone(), path: self.mirror(e), expect: e.remote, force };
        if let Some(e) = self.entry_mut(id) {
            e.in_flight = true;
            e.again = false;
            e.sync = Sync::Pending;
        }
        self.ops.push(op);
    }

    fn wrote_back(&mut self, id: u64, result: Result<Remote, WriteError>, app: &mut WordApp) {
        let Some(e) = self.entry(id) else { return };
        let name = e.display_name();
        let mirror = self.mirror(e);
        let open = app.session.path.as_deref() == Some(mirror.as_path());
        let (again, export) = (e.again, e.export);
        if let Some(e) = self.entry_mut(id) {
            e.in_flight = false;
        }
        match result {
            Ok(remote) => {
                if let Some(e) = self.entry_mut(id) {
                    e.remote = remote;
                    e.sync = Sync::Clean;
                    e.export = false;
                }
                if again {
                    self.write_back(id, false);
                } else if export && !open {
                    self.exports.push(Exported { mime: mime_for_name(&name).to_owned(), path: mirror, name });
                }
            }
            Err(WriteError::Conflict(now)) => {
                if let Some(e) = self.entry_mut(id) {
                    e.sync = Sync::Failed;
                    e.remote = Remote { modified: e.remote.modified, size: e.remote.size };
                }
                log::info!("{name} changed elsewhere ({now:?}): asking");
                if !self.questions.iter().any(|q| matches!(q, Question::Conflict { entry, .. } if *entry == id)) {
                    self.questions.push(Question::Conflict { entry: id, name });
                }
            }
            Err(WriteError::Lost(why) | WriteError::Failed(why)) => {
                if let Some(e) = self.entry_mut(id) {
                    e.sync = Sync::Failed;
                }
                self.unsaved(app, open, format!("Couldn't save to {name} ({why}). The changes are kept here: save again, or use Save As."));
            }
        }
    }

    /// The changes didn't reach the document: the open document is marked unsaved, and AutoSave (which
    /// would keep rewriting a copy that can't be written back) stops.
    fn unsaved(&mut self, app: &mut WordApp, open: bool, message: String) {
        if open {
            app.session.dirty = true;
            app.autosave = false;
        }
        log::warn!("{message}");
        self.notices.push(message);
    }

    /// The user's answer to a conflict.
    pub fn resolve(&mut self, entry: u64, choice: ConflictChoice, app: &mut WordApp) {
        self.questions.retain(|q| !matches!(q, Question::Conflict { entry: e, .. } if *e == entry));
        let Some(e) = self.entry(entry) else { return };
        let open = app.session.path.as_deref() == Some(self.mirror(e).as_path());
        let name = e.display_name();
        match choice {
            ConflictChoice::Replace => self.write_back(entry, true),
            ConflictChoice::SaveCopy => {
                if open {
                    app.session.dirty = true;
                    app.save_as_dialog();
                }
            }
            ConflictChoice::Keep => self.unsaved(app, open, format!("{name} wasn't replaced: the changes are kept here.")),
        }
    }

    /// After each frame: shows pickers the app asked for, notices saves to write back, checks a document
    /// just opened from Recent for newer versions.
    pub fn after_frame(&mut self, app: &mut WordApp) {
        // A document opened (from Recent, say): is the mirror still current?
        let now = (app.session.document_id(), app.session.path.clone());
        if self.document.as_ref() != Some(&now) {
            let opened = self.document.as_ref().is_some_and(|(id, _)| *id != now.0);
            self.document = Some(now);
            if opened
                && let Some(path) = &app.session.path
                && let Some(e) = self.entry_at(path)
                && e.sync == Sync::Clean
                && !e.in_flight
                && !self.copies.iter().any(|c| c.entry == Some(e.id))
            {
                let (id, uri, expect) = (e.id, e.uri.clone(), e.remote);
                let req = self.id();
                if let Some(copy) = self.document_copy(req, id, Then::Reopen) {
                    self.ops.push(FileOp::Refresh { req, uri, path: copy.dest.clone(), expect });
                    self.copies.push(copy);
                }
            }
        }
        // Saves: the open document's mirror, and files Save dialogs handed out.
        let open = app.session.path.as_deref().and_then(|p| self.entry_at(p)).map(|e| e.id);
        let watched: Vec<u64> = self.entries.iter().filter(|e| e.watch || Some(e.id) == open).map(|e| e.id).collect();
        for id in watched {
            let Some(e) = self.entry(id) else { continue };
            let mirror = self.mirror(e);
            let stamp = Stamp::of(&mirror);
            if stamp.is_none() || stamp == e.stamp {
                continue;
            }
            let (writable, in_flight) = (e.writable, e.in_flight);
            if let Some(e) = self.entry_mut(id) {
                e.stamp = stamp;
                e.export = e.watch && Some(e.id) != open;
                e.watch = false;
            }
            if !writable {
                let is_open = Some(id) == open;
                if let Some(e) = self.entry_mut(id) {
                    e.sync = Sync::Failed;
                }
                let name = self.entry(id).map(Entry::display_name).unwrap_or_default();
                self.unsaved(app, is_open, format!("{name} can't be changed from here: use Save As to save your changes."));
                if is_open {
                    app.save_as_dialog();
                }
            } else if in_flight {
                if let Some(e) = self.entry_mut(id) {
                    e.again = true;
                }
            } else {
                self.write_back(id, false);
            }
        }
        // Last: a save just noticed may have asked for Save As.
        self.take_dialogs();
        self.save();
    }

    /// Requests from the `file_dialog` hook become pickers.
    fn take_dialogs(&mut self) {
        let asked = std::mem::take(&mut self.dialogs.borrow_mut().asked);
        for (request, tx) in asked {
            if self.dialogs.borrow().pending.is_some() {
                // The app shows one dialog at a time; this can't happen, but must not hang it.
                answer(tx, Ok(None));
                continue;
            }
            let req = self.id();
            let op = match &request {
                FileDialogRequest::Open { purpose } => FileOp::Pick { req, create: None, mimes: mimes_for(purpose), persist: purpose == "document" },
                FileDialogRequest::Save { name } => {
                    let name = sanitize(name).unwrap_or_else(|| "Document1.docx".to_owned());
                    let mime = mime_for_name(&name).to_owned();
                    FileOp::Pick { req, create: Some((name, mime)), mimes: Vec::new(), persist: true }
                }
            };
            self.ops.push(op);
            self.dialogs.borrow_mut().pending = Some(Pending { req, request, tx, copying: false });
        }
    }

    /// What the platform should do now.
    pub fn take_ops(&mut self) -> Vec<FileOp> {
        std::mem::take(&mut self.ops)
    }

    /// Documents to open as dropped files.
    pub fn take_to_open(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.to_open)
    }

    /// Messages for the status bar.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Exports just written to their documents.
    pub fn take_exports(&mut self) -> Vec<Exported> {
        std::mem::take(&mut self.exports)
    }

    /// The question to show, if any.
    pub fn question(&self) -> Option<&Question> {
        self.questions.first()
    }

    /// Whether a write-back is under way.
    pub fn writing(&self) -> bool {
        self.entries.iter().any(|e| e.in_flight)
    }

    /// Whether a write-back is under way or failed.
    #[cfg(test)]
    pub fn unsynced(&self) -> bool {
        self.entries.iter().any(|e| e.in_flight || e.sync == Sync::Failed)
    }
}

fn answer(tx: Sender<FileDialogAnswer>, answer: FileDialogAnswer) {
    // The app may have given the dialog up: nothing to tell.
    let _ = tx.send(answer);
}

/// A file name safe in one folder: the last segment, no control characters, not hidden, not too long.
fn sanitize(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    if clean.is_empty() {
        return None;
    }
    let ext = Path::new(clean).extension().and_then(|e| e.to_str()).filter(|e| e.chars().count() <= 8);
    if clean.chars().count() <= MAX_NAME {
        return Some(clean.to_owned());
    }
    // Shortened, keeping the extension.
    let keep = MAX_NAME.saturating_sub(ext.map_or(0, |e| e.chars().count() + 1));
    let stem: String = clean.chars().take(keep).collect();
    Some(match ext {
        Some(e) => format!("{}.{e}", stem.trim_end()),
        None => stem.trim_end().to_owned(),
    })
}

/// The mirror's name for a document saved as `requested`: the provider's name (the user may have changed
/// it), with the requested extension, which picks the format.
fn save_name(provided: &str, requested: &str) -> String {
    let requested = sanitize(requested).unwrap_or_else(|| "Document1.docx".to_owned());
    let Some(name) = sanitize(provided) else { return requested };
    let ext = Path::new(&requested).extension().and_then(|e| e.to_str()).unwrap_or("docx");
    let has = Path::new(&name).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(ext));
    if has { name } else { sanitize(&format!("{name}.{ext}")).unwrap_or(requested) }
}

/// The mirror's name for an opened document: the provider's name, with an extension from its type when
/// it has none (the app picks the reader by extension, else by content).
fn open_name(provided: &str, mime: Option<&str>) -> String {
    let name = sanitize(provided).unwrap_or_else(|| "Document".to_owned());
    if Path::new(&name).extension().is_some() {
        return name;
    }
    match mime.and_then(ext_for_mime) {
        Some(ext) => format!("{name}.{ext}"),
        None => name,
    }
}

fn is_document_name(name: &str) -> bool {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    open_filters("document").first().is_some_and(|(_, exts)| exts.contains(&ext.as_str())) || ext.is_empty()
}

/// MIME types and extensions, for the picker's filter and for names without an extension.
const TYPES: &[(&str, &str)] = &[
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("docm", "application/vnd.ms-word.document.macroEnabled.12"),
    ("dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
    ("dotm", "application/vnd.ms-word.template.macroEnabled.12"),
    ("doc", "application/msword"),
    ("dot", "application/msword"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    // Android's own type for .rtf first: the one to create documents with.
    ("rtf", "text/rtf"),
    ("rtf", "application/rtf"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("tex", "application/x-tex"),
    ("json", "application/json"),
    ("xml", "text/xml"),
    ("xml", "application/xml"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("xlsm", "application/vnd.ms-excel.sheet.macroEnabled.12"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("ttc", "font/collection"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
];

fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    TYPES.iter().find(|(_, m)| m.eq_ignore_ascii_case(mime)).map(|(e, _)| *e)
}

/// The type to create a document of, from its name.
pub fn mime_for_name(name: &str) -> &'static str {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    TYPES.iter().find(|(e, _)| *e == ext).map_or("application/octet-stream", |(_, m)| m)
}

/// The picker's filter for an Open dialog: the types of the purpose's extensions. Providers often call
/// Markdown, TeX and fonts `text/*` variants or `application/octet-stream`, so those are offered too
/// (but for pictures).
fn mimes_for(purpose: &str) -> Vec<String> {
    let mut mimes: Vec<String> = Vec::new();
    for (_, exts) in open_filters(purpose) {
        for ext in *exts {
            for (e, m) in TYPES {
                if e == ext && !mimes.iter().any(|x| x == m) {
                    mimes.push((*m).to_owned());
                }
            }
        }
    }
    if purpose != "picture" {
        mimes.extend(["text/*".to_owned(), "application/octet-stream".to_owned()]);
    }
    mimes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::temp_dir;
    use wordcraft_doc::StoryRef;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::Services;

    /// An app wired to `files` as the Android host wires it.
    fn app(files: &Files) -> WordApp {
        let dialogs = files.dialogs();
        let services = Services { file_dialog: Some(Box::new(move |req| Dialogs::ask(&dialogs, req))), ..Services::default() };
        WordApp::new(Session::new(wordcraft_doc::Document::new()), services)
    }

    fn doc_uri(n: u32) -> String {
        format!("content://com.example.docs/document/{n}")
    }

    fn picked(n: u32, name: &str) -> Picked {
        Picked {
            uri: doc_uri(n),
            name: name.into(),
            mime: None,
            writable: true,
            persisted: true,
            is_virtual: false,
            remote: Remote { modified: Some(1000), size: Some(10) },
        }
    }

    /// The pick the app's last dialog asked for.
    fn pick_req(files: &mut Files, app: &mut WordApp) -> (u64, FileOp) {
        files.after_frame(app);
        let ops = files.take_ops();
        let op = ops.into_iter().find(|o| matches!(o, FileOp::Pick { .. })).expect("a picker");
        let FileOp::Pick { req, .. } = &op else { panic!("{op:?}") };
        (*req, op)
    }

    /// What the app does with a dialog's answer runs in `logic`; the tests call it as a frame would.
    fn settle(app: &mut WordApp) {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.logic(ui.ctx()));
        out.textures_delta.clear();
    }

    /// What Android's copy writes: a .docx with `text`, wherever the copy goes.
    fn write_doc(path: &Path, text: &str) {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("text.insert", &json!({"text": text})).unwrap();
        fs::write(path, wordcraft_engine::io::save_bytes("copy.docx", &s.doc).unwrap()).unwrap();
    }

    #[test]
    fn opening_copies_the_document_in_and_saving_writes_it_back() {
        let root = temp_dir("files-open");
        let mut files = Files::new(&root);
        assert_eq!(files.take_ops(), [FileOp::ListGrants]);
        let mut app = app(&files);
        app.run("file.open", json!({})).unwrap();
        settle(&mut app);
        let (req, op) = pick_req(&mut files, &mut app);
        let FileOp::Pick { create: None, mimes, persist: true, .. } = op else { panic!("{op:?}") };
        assert!(mimes.iter().any(|m| m.contains("wordprocessingml.document")));
        files.event(FileEvent::Picked { req, result: Ok(Some(picked(1, "Letter.docx"))) }, &mut app);
        let ops = files.take_ops();
        let [FileOp::CopyIn { req: copy, uri, path }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!((*copy, uri.as_str()), (req, doc_uri(1).as_str()));
        assert!(path.starts_with(root.join("docs")) && path.ends_with("Letter.docx"));
        // The platform copies the document in.
        write_doc(path, "Dear Ayu,");
        files.event(FileEvent::CopiedIn { req, result: Ok(Remote { modified: Some(1000), size: Some(10) }) }, &mut app);
        settle(&mut app);
        assert!(app.session.doc.plain_text(StoryRef::Body).contains("Dear Ayu,"));
        assert_eq!(app.session.path.as_deref(), Some(path.as_path()));
        files.after_frame(&mut app);
        assert!(files.take_ops().iter().all(|o| !matches!(o, FileOp::WriteBack { .. })), "opening isn't a save");

        // Typing then saving: written back, expecting the document as it was copied in.
        app.run("text.insert", json!({"text": "Hi "})).unwrap();
        app.run("file.save", json!({})).unwrap();
        files.after_frame(&mut app);
        let ops = files.take_ops();
        let [FileOp::WriteBack { entry, uri, path: from, expect, force: false }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!((uri.as_str(), from), (doc_uri(1).as_str(), path));
        assert_eq!(expect.modified, Some(1000));
        assert!(files.unsynced());
        // Saved again before the first write-back finished: one more after it.
        app.run("text.insert", json!({"text": "again "})).unwrap();
        app.run("file.save", json!({})).unwrap();
        files.after_frame(&mut app);
        assert!(files.take_ops().is_empty(), "one write-back at a time");
        files.event(FileEvent::WroteBack { entry: *entry, result: Ok(Remote { modified: Some(2000), size: Some(12) }) }, &mut app);
        let ops = files.take_ops();
        let [FileOp::WriteBack { expect, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(expect.modified, Some(2000));
        files.event(FileEvent::WroteBack { entry: *entry, result: Ok(Remote { modified: Some(3000), size: Some(14) }) }, &mut app);
        assert!(!files.unsynced());

        // The registry survives a restart; Recent's entry still opens.
        files.save();
        let mut again = Files::new(&root);
        let mut app2 = self::app(&again);
        app2.ui.recent = app.ui.recent.clone();
        again.collect_garbage(&mut app2);
        assert_eq!(app2.ui.recent, [path.to_string_lossy()]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_as_answers_with_an_empty_mirror_and_writes_back_the_save() {
        let root = temp_dir("files-save");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        app.run("text.insert", json!({"text": "Chapter one"})).unwrap();
        app.save_as_dialog();
        let (req, op) = pick_req(&mut files, &mut app);
        let FileOp::Pick { create: Some((name, mime)), .. } = op else { panic!("{op:?}") };
        assert_eq!(name, "Document1.docx");
        assert!(mime.contains("wordprocessingml.document"));
        // The user named it "Novel" (the provider drops the extension).
        files.event(FileEvent::Picked { req, result: Ok(Some(Picked { remote: Remote::default(), ..picked(7, "Novel") })) }, &mut app);
        settle(&mut app);
        let path = app.session.path.clone().expect("saved");
        assert!(path.ends_with("Novel.docx"), "{path:?}");
        assert!(!app.session.dirty);
        files.after_frame(&mut app);
        let ops = files.take_ops();
        assert!(matches!(ops.as_slice(), [FileOp::WriteBack { uri, force: false, .. }] if *uri == doc_uri(7)), "{ops:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exports_are_written_back_once() {
        let root = temp_dir("files-export");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        app.run("text.insert", json!({"text": "Report"})).unwrap();
        // File › Export › PDF (what `export_dialog` asks).
        let rx = Dialogs::ask(&files.dialogs(), FileDialogRequest::Save { name: "Report.pdf".into() });
        let (req, op) = pick_req(&mut files, &mut app);
        let FileOp::Pick { create: Some((_, mime)), .. } = op else { panic!() };
        assert_eq!(mime, "application/pdf");
        files.event(FileEvent::Picked { req, result: Ok(Some(picked(9, "Report.pdf"))) }, &mut app);
        let path = rx.try_recv().unwrap().unwrap().unwrap();
        app.run("file.saveAs", json!({"path": path})).unwrap();
        files.after_frame(&mut app);
        let ops = files.take_ops();
        let [FileOp::WriteBack { entry, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        files.after_frame(&mut app);
        assert!(files.take_ops().is_empty(), "no longer watched");
        files.event(FileEvent::WroteBack { entry: *entry, result: Ok(Remote::default()) }, &mut app);
        let exports = files.take_exports();
        assert_eq!(exports.len(), 1, "offered for sharing");
        assert_eq!((exports[0].name.as_str(), exports[0].mime.as_str()), ("Report.pdf", "application/pdf"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_and_failed_pickers() {
        let root = temp_dir("files-cancel");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        let rx = Dialogs::ask(&files.dialogs(), FileDialogRequest::Open { purpose: "document".into() });
        let (req, _) = pick_req(&mut files, &mut app);
        files.event(FileEvent::Picked { req, result: Ok(None) }, &mut app);
        assert_eq!(rx.try_recv().unwrap(), Ok(None));
        // No picker on this device: the app's own browser.
        let rx = Dialogs::ask(&files.dialogs(), FileDialogRequest::Open { purpose: "document".into() });
        let (req, _) = pick_req(&mut files, &mut app);
        files.event(FileEvent::Picked { req, result: Err("no picker".into()) }, &mut app);
        assert_eq!(rx.try_recv().unwrap(), Err("no picker".into()));
        // A copy-in that fails: cancelled, with a message.
        let rx = Dialogs::ask(&files.dialogs(), FileDialogRequest::Open { purpose: "picture".into() });
        let (req, op) = pick_req(&mut files, &mut app);
        assert!(matches!(&op, FileOp::Pick { persist: false, mimes, .. } if mimes.contains(&"image/png".to_owned())));
        files.event(FileEvent::Picked { req, result: Ok(Some(picked(3, "cat.png"))) }, &mut app);
        assert!(matches!(files.take_ops().as_slice(), [FileOp::CopyIn { path, .. }] if path.starts_with(root.join("imports"))));
        files.event(FileEvent::CopiedIn { req, result: Err("gone".into()) }, &mut app);
        assert_eq!(rx.try_recv().unwrap(), Ok(None));
        assert_eq!(files.take_notices().len(), 1);
        // Stale answers are ignored.
        files.event(FileEvent::Picked { req: 999, result: Ok(Some(picked(4, "x.docx"))) }, &mut app);
        files.event(FileEvent::CopiedIn { req: 999, result: Ok(Remote::default()) }, &mut app);
        assert!(files.take_ops().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_write_back_keeps_the_document_unsaved() {
        let root = temp_dir("files-fail");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        app.save_as_dialog();
        let (req, _) = pick_req(&mut files, &mut app);
        files.event(FileEvent::Picked { req, result: Ok(Some(picked(5, "a.docx"))) }, &mut app);
        settle(&mut app);
        files.after_frame(&mut app);
        let ops = files.take_ops();
        let [FileOp::WriteBack { entry, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert!(!app.session.dirty);
        files.event(FileEvent::WroteBack { entry: *entry, result: Err(WriteError::Lost("no permission".into())) }, &mut app);
        assert!(app.session.dirty, "closing asks to save");
        assert!(!app.autosave);
        assert!(files.take_notices()[0].contains("no permission"));
        assert!(files.unsynced());
        // A conflict asks.
        app.run("file.save", json!({})).unwrap();
        files.after_frame(&mut app);
        files.take_ops();
        files.event(FileEvent::WroteBack { entry: *entry, result: Err(WriteError::Conflict(Remote::default())) }, &mut app);
        assert!(matches!(files.question(), Some(Question::Conflict { name, .. }) if name == "a.docx"));
        files.resolve(*entry, ConflictChoice::Replace, &mut app);
        assert!(files.question().is_none());
        assert!(matches!(files.take_ops().as_slice(), [FileOp::WriteBack { force: true, .. }]));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_read_only_document_asks_where_to_save() {
        let root = temp_dir("files-ro");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        files.event(FileEvent::Incoming { picked: Picked { writable: false, persisted: false, ..picked(8, "Shared.docx") } }, &mut app);
        let ops = files.take_ops();
        let [FileOp::CopyIn { req, path, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        write_doc(path, "shared text");
        files.event(FileEvent::CopiedIn { req: *req, result: Ok(Remote::default()) }, &mut app);
        let open = files.take_to_open();
        assert_eq!(open, std::slice::from_ref(path));
        app.run("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        files.after_frame(&mut app);
        files.take_ops();
        app.run("text.insert", json!({"text": "edit "})).unwrap();
        app.run("file.save", json!({})).unwrap();
        files.after_frame(&mut app);
        assert!(app.session.dirty);
        assert!(app.file_dialog_open(), "Save As is asking");
        assert!(files.take_ops().iter().any(|o| matches!(o, FileOp::Pick { create: Some(_), .. })));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lost_permissions_and_garbage() {
        let root = temp_dir("files-gc");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        let mut paths = Vec::new();
        for n in 0..3 {
            files.event(FileEvent::Incoming { picked: picked(n, &format!("d{n}.docx")) }, &mut app);
            let ops = files.take_ops();
            let [FileOp::CopyIn { req, path, .. }] = ops.as_slice() else { panic!() };
            write_doc(path, "x");
            files.event(FileEvent::CopiedIn { req: *req, result: Ok(Remote::default()) }, &mut app);
            paths.push(path.clone());
        }
        app.ui.recent =
            vec![paths[0].to_string_lossy().into(), "/elsewhere/kept.docx".into(), root.join("docs/999/gone.docx").to_string_lossy().into()];
        app.session.path = Some(paths[1].clone());
        files.event(FileEvent::Grants { uris: vec![doc_uri(1), doc_uri(2)] }, &mut app);
        assert_eq!(app.ui.recent, [paths[0].to_string_lossy().as_ref(), "/elsewhere/kept.docx"]);
        assert!(paths[0].is_file() && paths[1].is_file());
        assert!(!paths[2].exists(), "a document nothing lists is forgotten");
        let ops = files.take_ops();
        assert_eq!(ops, [FileOp::Release { uri: doc_uri(2) }]);
        // Document 0's permission is gone: saving it asks for a new place.
        assert!(!files.entry_at(&paths[0]).unwrap().writable);
        assert!(files.entry_at(&paths[1]).unwrap().writable);
        let _ = fs::remove_dir_all(root);
    }

    /// A document opened from Android and saved here, its write-back done.
    fn opened_and_synced(files: &mut Files, app: &mut WordApp, n: u32, text: &str) -> (u64, PathBuf) {
        files.after_frame(app);
        files.event(FileEvent::Incoming { picked: picked(n, &format!("d{n}.docx")) }, app);
        let ops = files.take_ops();
        let [FileOp::CopyIn { req, path, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        write_doc(path, text);
        files.event(FileEvent::CopiedIn { req: *req, result: Ok(Remote { modified: Some(1), size: Some(1) }) }, app);
        let path = files.take_to_open().pop().unwrap();
        app.run("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        files.after_frame(app);
        // Opening asks whether there's a newer version: there isn't.
        let ops = files.take_ops();
        let [FileOp::Refresh { req, path: side, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_ne!(side, &path, "a refresh copies beside the mirror");
        files.event(FileEvent::Refreshed { req: *req, result: Ok(None) }, app);
        let id = files.entry_at(&path).unwrap().id;
        (id, path)
    }

    #[test]
    fn open_with_never_copies_over_changes_still_to_write_back() {
        let root = temp_dir("files-unsynced");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        let (id, path) = opened_and_synced(&mut files, &mut app, 1, "first");
        app.run("text.insert", json!({"text": "mine "})).unwrap();
        app.run("file.save", json!({})).unwrap();
        files.after_frame(&mut app);
        files.take_ops();
        files.event(FileEvent::WroteBack { entry: id, result: Err(WriteError::Failed("offline".into())) }, &mut app);
        let saved = fs::read(&path).unwrap();
        // The same document, sent again from the Files app.
        files.event(FileEvent::Incoming { picked: picked(1, "d1.docx") }, &mut app);
        assert!(files.take_ops().iter().all(|o| !matches!(o, FileOp::CopyIn { .. })), "nothing is copied over the changes");
        assert_eq!(files.take_to_open(), std::slice::from_ref(&path));
        assert_eq!(fs::read(&path).unwrap(), saved);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_newer_version_found_while_editing_waits_for_the_conflict() {
        let root = temp_dir("files-refresh");
        let mut files = Files::new(&root);
        files.take_ops();
        let mut app = app(&files);
        let (id, path) = opened_and_synced(&mut files, &mut app, 2, "first");
        // Opened again from Recent while the other device's version downloads; the user types meanwhile.
        app.run("file.new", json!({})).unwrap();
        files.after_frame(&mut app);
        app.run("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        files.after_frame(&mut app);
        let ops = files.take_ops();
        let [FileOp::Refresh { req, path: side, expect, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(expect.modified, Some(1));
        app.run("text.insert", json!({"text": "typed meanwhile "})).unwrap();
        write_doc(side, "theirs");
        files.event(FileEvent::Refreshed { req: *req, result: Ok(Some(Remote { modified: Some(2), size: Some(2) })) }, &mut app);
        assert!(!side.exists());
        assert!(app.session.doc.plain_text(StoryRef::Body).contains("typed meanwhile"), "the edits stay");
        assert_eq!(files.entry(id).unwrap().remote.modified, Some(1), "so the next write-back finds the conflict");
        // Untouched, the newer version is opened.
        app.session.dirty = false;
        app.run("file.new", json!({})).unwrap();
        files.after_frame(&mut app);
        app.run("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        files.after_frame(&mut app);
        let ops = files.take_ops();
        let [FileOp::Refresh { req, path: side, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        write_doc(side, "theirs");
        files.event(FileEvent::Refreshed { req: *req, result: Ok(Some(Remote { modified: Some(2), size: Some(2) })) }, &mut app);
        assert!(app.session.doc.plain_text(StoryRef::Body).contains("theirs"));
        assert_eq!(files.entry(id).unwrap().remote.modified, Some(2));
        files.after_frame(&mut app);
        assert!(files.take_ops().iter().all(|o| !matches!(o, FileOp::WriteBack { .. })), "a copy-in isn't a save");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(sanitize("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize("..\\x.docx").as_deref(), Some("x.docx"));
        assert_eq!(sanitize(".hidden.docx").as_deref(), Some("hidden.docx"));
        assert_eq!(sanitize(" \u{0}\n ").as_deref(), None);
        let long = format!("{}.docx", "é".repeat(300));
        let s = sanitize(&long).unwrap();
        assert!(s.ends_with(".docx") && s.chars().count() <= MAX_NAME);
        assert_eq!(save_name("Novel", "Document1.docx"), "Novel.docx");
        assert_eq!(save_name("Novel.DOCX", "Document1.docx"), "Novel.DOCX");
        assert_eq!(save_name("Report.docx", "Report.pdf"), "Report.docx.pdf");
        assert_eq!(save_name("", "x.pdf"), "x.pdf");
        assert_eq!(open_name("notes", Some("text/markdown; charset=utf-8")), "notes.md");
        assert_eq!(open_name("blob", Some("application/x-unknown")), "blob");
        assert_eq!(mime_for_name("a.ODT"), "application/vnd.oasis.opendocument.text");
        assert!(mimes_for("recipients").contains(&"text/csv".to_owned()));
        assert!(!mimes_for("picture").contains(&"application/octet-stream".to_owned()));
        assert!(is_document_name("a.md") && !is_document_name("a.png"));
    }

    #[test]
    fn hostile_registries_are_ignored() {
        let root = temp_dir("files-hostile");
        let docs = root.join("docs");
        fs::create_dir_all(docs.join("1")).unwrap();
        fs::write(docs.join("1").join("a.docx"), b"x").unwrap();
        for bad in [
            json!({"version": 1, "documents": [{"id": 1, "uri": "u", "file": "../../ui.json"}]}),
            json!({"version": 1, "documents": [{"id": 1, "uri": "u", "file": "/etc/passwd"}]}),
            json!({"version": 1, "documents": [{"id": 1, "uri": "u", "file": "1/../../x"}]}),
            json!({"version": 1, "documents": [{"id": 2, "uri": "u", "file": "2/missing.docx"}]}),
            json!({"version": 1, "documents": "nope"}),
            json!({"version": 1, "next": u64::MAX, "documents": [{"id": u64::MAX, "uri": "u", "file": "1/a.docx"}]}),
        ] {
            write_atomic(&docs.join("docs.json"), &serde_json::to_vec(&bad).unwrap()).unwrap();
            let mut files = Files::new(&root);
            assert!(files.entries.iter().all(|e| files.mirror(e).starts_with(&docs) && files.mirror(e).is_file()), "{bad}");
            let _ = files.id();
        }
        let _ = fs::remove_dir_all(root);
    }
}
