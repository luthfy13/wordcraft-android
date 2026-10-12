//! The app's own files, in its private storage (`Context.getFilesDir()/wordcraft`): the settings the
//! desktop app keeps in its settings folder (`ui.json`, the custom dictionary, added fonts, Hunspell
//! dictionaries) and the recovery snapshot (`recovery.rs`).
//!
//! Android can kill the app at any moment, so a file that's replaced is written beside it and renamed
//! over it: a half-written `ui.json` would lose every setting and the recent documents.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wordcraft_engine::Session;
use wordcraft_ui_egui::{UiState, WordApp};

/// Largest `ui.json` read: real ones are a few kilobytes.
const MAX_PREFS: u64 = 4 << 20;

/// Where the app keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    root: PathBuf,
}

impl Dirs {
    /// The folders in `files_dir`, the app's private storage.
    pub fn new(files_dir: &Path) -> Self {
        Self { root: files_dir.join("wordcraft") }
    }

    /// The folder itself (documents live in `docs/` there, see `files.rs`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// PDFs handed to Android's print service.
    pub fn print(&self) -> PathBuf {
        self.root.join("print")
    }

    /// The settings, as the desktop app's `ui.json`.
    pub fn prefs(&self) -> PathBuf {
        self.root.join("ui.json")
    }

    /// The words Add to Dictionary kept.
    pub fn dictionary(&self) -> PathBuf {
        self.root.join(Session::USER_DICT_FILE)
    }

    /// Fonts the user added.
    pub fn fonts(&self) -> PathBuf {
        self.root.join("fonts")
    }

    /// Hunspell dictionaries (`.aff` and `.dic`), searched before the system's (Android has none).
    pub fn hunspell(&self) -> PathBuf {
        self.root.join("dictionaries")
    }

    /// The snapshot of a document with unsaved changes.
    pub fn recovery(&self) -> PathBuf {
        self.root.join("recovery")
    }
}

/// Loads what lives outside `ui.json` and must be in place before a document is laid out: added fonts and
/// the Hunspell folder. Process-wide, like the desktop app's.
pub fn load_fonts_and_dictionaries(dirs: &Dirs) {
    let (loaded, skipped) = wordcraft_fonts::FontDb::global().load_user_fonts(&dirs.fonts());
    if !loaded.is_empty() || !skipped.is_empty() {
        log::info!("added fonts: {} loaded, {} skipped", loaded.len(), skipped.len());
    }
    wordcraft_engine::proof::hunspell::set_dictionary_dir(Some(dirs.hunspell()));
}

/// Applies the saved settings and the custom dictionary. A missing or unreadable file leaves the defaults.
pub fn load_prefs(dirs: &Dirs, app: &mut WordApp) {
    match read_capped(&dirs.prefs(), MAX_PREFS) {
        Ok(Some(bytes)) => match UiState::from_json(&bytes) {
            Some(ui) => app.apply_prefs(ui),
            None => log::warn!("the settings file isn't valid: using the defaults"),
        },
        Ok(None) => {}
        Err(e) => log::warn!("can't read the settings: {e}"),
    }
    let dictionary = dirs.dictionary();
    wordcraft_engine::proof::load_user_dictionary(&dictionary);
    app.session.user_dict_path = Some(dictionary);
}

/// Writes the settings (atomically).
pub fn save_prefs(dirs: &Dirs, app: &WordApp) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&app.prefs()).map_err(io::Error::other)?;
    write_atomic(&dirs.prefs(), &bytes)
}

/// Reads a file of at most `max` bytes; `None` when there's none.
fn read_capped(path: &Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Ok(m) if m.len() > max => Err(io::Error::other(format!("{} is larger than {max} bytes", path.display()))),
        Ok(_) => fs::read(path).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replaces `path` with `bytes`, or leaves it as it was: written to a new file beside it, synced, then
/// renamed over it. Makes the folder if needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no folder"))?;
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    fs::create_dir_all(dir)?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let temp = dir.join(temp_name);
    let written = OpenOptions::new().write(true).create_new(true).open(&temp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    match written.and_then(|()| fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wordcraft_ui_egui::Services;

    /// A new, empty folder for one test.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("folio-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app() -> WordApp {
        WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default())
    }

    #[test]
    fn atomic_writes_replace_the_whole_file_and_leave_nothing_behind() {
        let dir = temp_dir("atomic");
        let path = dir.join("a").join("ui.json");
        write_atomic(&path, b"first, longer").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["ui.json"], "no temporary file is left");
        // A folder in the way: the write fails and the folder stays.
        fs::create_dir_all(dir.join("b").join("ui.json").join("x")).unwrap();
        assert!(write_atomic(&dir.join("b").join("ui.json"), b"x").is_err());
        assert_eq!(fs::read_dir(dir.join("b")).unwrap().count(), 1, "the failed write's temporary file is gone");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_survive_a_restart() {
        let dir = temp_dir("prefs");
        let dirs = Dirs::new(&dir);
        let mut first = app();
        load_prefs(&dirs, &mut first);
        first.ui.recent = vec!["/data/doc.docx".into()];
        first.ui.recent_count = 7;
        first.session.author = "Ayu".into();
        save_prefs(&dirs, &first).unwrap();

        let mut second = app();
        load_prefs(&dirs, &mut second);
        assert_eq!(second.ui.recent, ["/data/doc.docx"]);
        assert_eq!(second.ui.recent_count, 7);
        assert_eq!(second.session.author, "Ayu");
        assert_eq!(second.session.user_dict_path.as_deref(), Some(dirs.dictionary().as_path()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_settings_file_leaves_the_defaults() {
        let dir = temp_dir("broken");
        let dirs = Dirs::new(&dir);
        write_atomic(&dirs.prefs(), b"{\"recent\": [\"/x.docx\"").unwrap();
        let mut app = app();
        load_prefs(&dirs, &mut app);
        assert!(app.ui.recent.is_empty());
        assert_eq!(app.ui.recent_count, UiState::default().recent_count);
        let _ = fs::remove_dir_all(dir);
    }
}
