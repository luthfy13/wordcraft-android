//! The platform-independent half of the Android host.
//!
//! [`Host`] owns egui's context and the WordCraft app, turns input into egui events and runs a frame the
//! way eframe does: `raw_input_hook`, then `logic` and `ui` in one pass. It knows nothing about Android
//! or the GPU, so tests drive it on the host.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::output::OutputCommand;
use egui::{ClippedPrimitive, CursorIcon, Event, RawInput, Rect, TexturesDelta, Theme, ViewportId, ViewportInfo, pos2};
use serde_json::json;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Services, WordApp};

use crate::files::{FileEvent, FileOp, Files};
use crate::input::{KeyPress, Motion, Translator};
use crate::keymap::code;
use crate::platform::{self, Inbound, Platform};
use crate::prompts::Prompts;
use crate::recovery::Recovery;
use crate::store::{self, Dirs};

/// How long quitting waits for documents to be written back before giving up.
const QUIT_WAIT: Duration = Duration::from_secs(15);

/// What to open at start-up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// Open the sample document (the desktop app's `--sample`).
    pub sample: bool,
}

impl Launch {
    /// Reads the desktop app's command-line flags (the activity passes them as one string, split on
    /// whitespace). Unknown flags are ignored.
    pub fn parse(args: &str) -> Self {
        Self { sample: args.split_whitespace().any(|a| a == "--sample") }
    }
}

/// The window, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub width: u32,
    pub height: u32,
    /// The display's pixels per point: its density over 160 dpi.
    pub native_pixels_per_point: f32,
    /// What the system bars and display cutouts cover: left, top, right, bottom.
    pub insets: [u32; 4],
}

impl Window {
    fn native_ppp(&self) -> f32 {
        let p = self.native_pixels_per_point;
        if p.is_finite() && p > 0.0 { p } else { 1.0 }
    }
}

/// One frame's output for the renderer and the platform.
pub struct Frame {
    pub primitives: Vec<ClippedPrimitive>,
    pub textures: TexturesDelta,
    pub pixels_per_point: f32,
}

/// The WordCraft app and egui, driven by the platform.
pub struct Host {
    ctx: egui::Context,
    app: WordApp,
    input: Translator,
    events: Vec<Event>,
    /// Text copied in the app: the clipboard without a platform (tests), and what's pasted when the
    /// system clipboard can't be read.
    clipboard: Rc<RefCell<String>>,
    /// Android, or `None` (tests: the app's own file browser and clipboard).
    platform: Option<Rc<dyn Platform>>,
    files: Option<Files>,
    /// Documents to hand the app as dropped files in the next frame (Open with, Share).
    dropped: Vec<PathBuf>,
    prompts: Prompts,
    /// The pointer's shape last sent.
    cursor: Option<CursorIcon>,
    /// The app's state when Back was pressed: if Back (as Esc) changes nothing, the app goes to the
    /// background.
    back: Option<BackState>,
    /// Since when quitting waits for write-backs.
    quitting: Option<Instant>,
    start: Instant,
    window: Option<Window>,
    focused: bool,
    system_theme: Option<Theme>,
    max_texture_side: usize,
    /// The app's files; `None` keeps nothing between runs (tests).
    dirs: Option<Dirs>,
    recovery: Option<Recovery>,
}

impl Host {
    /// The app, with the settings and any unsaved document a previous run left in `dirs`.
    pub fn new(launch: &Launch, dirs: Option<Dirs>, platform: Option<Rc<dyn Platform>>) -> Self {
        let clipboard = Rc::new(RefCell::new(String::new()));
        // Added fonts before the first layout, as on the desktop.
        if let Some(dirs) = &dirs {
            store::load_fonts_and_dictionaries(dirs);
        }
        let files = dirs.as_ref().filter(|_| platform.is_some()).map(|d| Files::new(d.root()));
        let doc = if launch.sample { wordcraft_engine::sample::sample_document() } else { wordcraft_doc::Document::new() };
        let mut app = WordApp::new(Session::new(doc), services(&clipboard, platform.as_ref(), files.as_ref(), dirs.as_ref()));
        let mut recovery = None;
        if let Some(dirs) = &dirs {
            store::load_prefs(dirs, &mut app);
            let mut r = Recovery::new(dirs.recovery());
            if let Some(message) = r.restore(&mut app) {
                log::info!("{message}");
                app.status(message);
            }
            recovery = Some(r);
        }
        // The ArtCraft brand licence (docs/brand/LICENSE-brand.txt) keeps its marks out of modified
        // versions: the "Join the ArtCraft community" button stays hidden, whatever the settings say.
        // About still names ArtCraft (that needs a hook in ui-egui).
        app.ui.show_discord = false;
        let mut host = Self {
            ctx: new_context(),
            app,
            input: Translator::default(),
            events: Vec::new(),
            clipboard,
            start: Instant::now(),
            window: None,
            focused: true,
            system_theme: None,
            max_texture_side: 2048,
            dirs,
            recovery,
            platform,
            files,
            dropped: Vec::new(),
            prompts: Prompts::default(),
            cursor: None,
            back: None,
            quitting: None,
        };
        host.pump();
        host
    }

    /// Keeps the recovery snapshot up to date; call after each frame and when [`Host::maintain`]'s time
    /// comes. Returns when to call again.
    pub fn maintain(&mut self, now: Instant) -> Option<Instant> {
        self.recovery.as_mut()?.observe(&mut self.app, now)
    }

    /// The app is leaving the screen (Pause, Stop) or going away (Destroy) and may be killed without
    /// warning: the settings and a snapshot of unsaved changes are written.
    pub fn persist(&mut self) {
        if let Some(r) = &mut self.recovery {
            r.flush(&mut self.app);
        }
        if let Some(files) = &mut self.files {
            files.save();
        }
        self.save_prefs();
    }

    /// A report from the platform.
    pub fn event(&mut self, inbound: Inbound) {
        if let Some(files) = &mut self.files {
            match inbound {
                Inbound::File(event) => files.event(event, &mut self.app),
                Inbound::Incoming(docs) => {
                    for picked in docs {
                        files.event(FileEvent::Incoming { picked }, &mut self.app);
                    }
                }
            }
        }
        self.pump();
        self.ctx.request_repaint();
    }

    /// Whether the app may close now that it asked to (File › Exit): not while a document is being
    /// written back. If writing back failed, or takes too long, closing is cancelled and the user told.
    pub fn ready_to_quit(&mut self) -> bool {
        let Some(files) = &self.files else { return true };
        if !files.unsynced() {
            return true;
        }
        let since = *self.quitting.get_or_insert_with(Instant::now);
        if files.writing() && since.elapsed() < QUIT_WAIT {
            return false;
        }
        self.quitting = None;
        self.app.quit_requested = false;
        self.app.status("Not closed: a document couldn't be saved to its file. Save it again or use Save As.");
        self.ctx.request_repaint();
        false
    }

    /// The user quit (File › Exit, or closing the document): the settings are written and nothing is kept
    /// to recover, the user having saved or discarded the changes. Waits up to `timeout` for the files.
    pub fn quit(&mut self, timeout: std::time::Duration) {
        self.save_prefs();
        if let Some(r) = &mut self.recovery {
            r.discard();
        }
        self.wait_for_files(timeout);
    }

    /// Waits up to `timeout` for the recovery snapshot to be written; whether it was.
    pub fn wait_for_files(&self, timeout: std::time::Duration) -> bool {
        self.recovery.as_ref().is_none_or(|r| r.wait(timeout))
    }

    fn save_prefs(&self) {
        if let Some(dirs) = &self.dirs
            && let Err(e) = store::save_prefs(dirs, &self.app)
        {
            log::error!("can't save the settings: {e}");
        }
    }

    /// egui's context, for the repaint callback.
    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// A new egui context, for a new graphics device: egui keeps no texture pixels, so a new renderer
    /// needs every texture sent again. The document, its undo history and the interface settings stay.
    /// Register the repaint callback on the new context.
    pub fn reset_context(&mut self) {
        let prefs = self.app.prefs();
        let session = std::mem::replace(&mut self.app.session, Session::new(wordcraft_doc::Document::new()));
        let mut app = WordApp::new(session, services(&self.clipboard, self.platform.as_ref(), self.files.as_ref(), self.dirs.as_ref()));
        app.apply_prefs(prefs);
        app.ui.show_discord = false;
        self.app = app;
        self.ctx = new_context();
        self.input = Translator::default();
        self.events.clear();
    }

    /// What shows where egui draws nothing (while the window resizes): the panels' colour.
    pub fn clear_color(&self) -> egui::Color32 {
        self.ctx.global_style().visuals.panel_fill
    }

    #[cfg(test)]
    pub fn app(&self) -> &WordApp {
        &self.app
    }

    #[cfg(test)]
    pub fn app_mut(&mut self) -> &mut WordApp {
        &mut self.app
    }

    /// The window appeared, changed or went away (`None`: frames are skipped until it's back).
    pub fn set_window(&mut self, window: Option<Window>) {
        if window.is_none() {
            self.input.reset(&mut self.events);
        }
        self.window = window;
    }

    pub fn window(&self) -> Option<Window> {
        self.window
    }

    pub fn set_focused(&mut self, focused: bool) {
        if focused != self.focused {
            self.focused = focused;
            if !focused {
                self.input.reset(&mut self.events);
            }
            self.events.push(Event::WindowFocused(focused));
        }
    }

    /// The system's light or dark mode, which the app's "Use system setting" follows.
    pub fn set_system_theme(&mut self, theme: Option<Theme>) {
        self.system_theme = theme;
    }

    /// The largest texture the GPU takes (page images are capped to it).
    pub fn set_max_texture_side(&mut self, side: usize) {
        self.max_texture_side = side.max(1);
    }

    /// The current pixels per point: the display's, times the app's interface size.
    pub fn pixels_per_point(&self) -> f32 {
        let native = self.window.map_or(1.0, |w| w.native_ppp());
        let zoom = self.ctx.zoom_factor();
        let ppp = native * zoom;
        if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 }
    }

    pub fn motion(&mut self, m: &Motion<'_>) {
        let ppp = self.pixels_per_point();
        self.input.motion(m, ppp, &mut self.events);
    }

    /// Android 12–13, whose touchpads send a tap-drag without a button (see `input.rs`).
    pub fn set_legacy_touchpad(&mut self, legacy: bool) {
        self.input.set_legacy_touchpad(legacy);
    }

    /// Sets egui's modifiers from the newest input event's meta state.
    pub fn sync_modifiers(&mut self, meta_state: u32) {
        self.input.sync_modifiers(meta_state, &mut self.events);
    }

    pub fn key(&mut self, k: &KeyPress) {
        if k.keycode == code::BACK && k.pressed && self.back.is_none() {
            self.back = Some(BackState::of(&self.app, &self.ctx));
        }
        let (clipboard, platform) = (&self.clipboard, &self.platform);
        self.input.key(k, || clipboard_text(platform.as_ref(), clipboard), &mut self.events);
    }

    /// Whether input is waiting for a frame.
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// Runs one frame, or `None` without a window.
    pub fn frame(&mut self) -> Option<Frame> {
        let window = self.window?;
        let native = window.native_ppp();
        let ppp = self.pixels_per_point();
        let [left, top, right, bottom] = window.insets.map(|i| i as f32 / ppp);
        let size = egui::vec2(window.width as f32, window.height as f32) / ppp;
        // The app lays out in the whole screen rect and doesn't read egui's safe area, so the rect
        // itself leaves out the system bars.
        let content = Rect::from_min_max(pos2(left, top), pos2(size.x - right, size.y - bottom));
        let mut raw = RawInput {
            viewport_id: ViewportId::ROOT,
            screen_rect: (content.width() > 0.0 && content.height() > 0.0).then_some(content),
            max_texture_side: Some(self.max_texture_side),
            time: Some(self.start.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            focused: self.focused,
            system_theme: self.system_theme,
            ..RawInput::default()
        };
        raw.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo { native_pixels_per_point: Some(native), focused: Some(self.focused), minimized: Some(false), ..ViewportInfo::default() },
        );
        raw.dropped_files = self.dropped.drain(..).map(|path| Arc::new(LocalFile(path)) as egui::DroppedFileHandle).collect();
        self.app.raw_input_hook(&mut raw);
        let (app, prompts, files, platform) = (&mut self.app, &mut self.prompts, &mut self.files, self.platform.as_deref());
        let out = self.ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
            prompts.show(ui, app, files.as_mut(), platform);
        });
        for command in out.platform_output.commands {
            match command {
                OutputCommand::CopyText(text) => {
                    if let Some(p) = &self.platform {
                        p.call("clipboardSet", json!({"text": text}));
                    }
                    *self.clipboard.borrow_mut() = text;
                }
                OutputCommand::OpenUrl(url) => {
                    if let Some(p) = &self.platform {
                        p.call("openUrl", json!({"url": url.url}));
                    }
                }
                OutputCommand::CopyImage(_) => {}
            }
        }
        let cursor = out.platform_output.cursor_icon;
        if self.cursor != Some(cursor) {
            self.cursor = Some(cursor);
            if let Some(p) = &self.platform {
                p.call("pointerIcon", json!({"type": platform::pointer_icon(cursor)}));
            }
        }
        if let Some(before) = self.back.take()
            && before == BackState::of(&self.app, &self.ctx)
            && let Some(p) = &self.platform
        {
            // Back with nothing to close: the app goes to the background, as at any app's root.
            p.call("moveTaskToBack", json!({}));
        }
        self.pump();
        let primitives = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        Some(Frame { primitives, textures: out.textures_delta, pixels_per_point: out.pixels_per_point })
    }

    /// Hands the platform what the documents need (pickers, copies, write-backs) and the app what came of
    /// it (documents to open, messages).
    fn pump(&mut self) {
        let Some(files) = &mut self.files else { return };
        files.after_frame(&mut self.app);
        // A synchronous answer can produce more work: a few rounds at most.
        for _ in 0..4 {
            let ops = files.take_ops();
            if ops.is_empty() {
                break;
            }
            for op in ops {
                let (name, args) = platform::file_op(&op);
                let answer = self.platform.as_ref().and_then(|p| p.call(name, args));
                if op == FileOp::ListGrants
                    && let Some(uris) = answer.as_ref().and_then(platform::grants)
                {
                    files.event(FileEvent::Grants { uris }, &mut self.app);
                }
            }
        }
        self.dropped.extend(files.take_to_open());
        if let Some(message) = files.take_notices().pop() {
            self.app.status(message);
        }
        let now = self.ctx.input(|i| i.time);
        for exported in files.take_exports() {
            self.prompts.exported(exported, now);
        }
        if !self.dropped.is_empty() {
            self.ctx.request_repaint();
        }
    }

    /// Whether the app asked to quit (File › Close on the last document, or Exit).
    pub fn quit_requested(&self) -> bool {
        self.app.quit_requested
    }
}

/// What Back (sent as Esc) may close or end; when none of it changes, Back leaves the app.
#[derive(PartialEq)]
struct BackState {
    dialog: bool,
    file_dialog: bool,
    backstage: bool,
    colors: bool,
    popup: bool,
    math: bool,
    painter: bool,
    read_mode: bool,
    sel: wordcraft_engine::Selection,
    modes: String,
}

impl BackState {
    fn of(app: &WordApp, ctx: &egui::Context) -> Self {
        Self {
            dialog: app.dialog.is_some(),
            file_dialog: app.file_dialog_open(),
            backstage: app.ui.backstage,
            colors: app.colors.is_some(),
            popup: egui::Popup::is_any_open(ctx),
            math: app.session.math.is_some(),
            painter: app.session.painter.is_some(),
            read_mode: app.session.view.read_mode,
            sel: app.session.sel.clone(),
            modes: format!("{:?} {:?}", app.session.view.draw.mode, app.ui.keytips),
        }
    }
}

/// A document handed to the app as a dropped file.
#[derive(Debug)]
struct LocalFile(PathBuf);

impl egui::DroppedFile for LocalFile {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// The system clipboard's text, else what was copied in the app.
fn clipboard_text(platform: Option<&Rc<dyn Platform>>, own: &Rc<RefCell<String>>) -> Option<String> {
    let system = platform.and_then(|p| p.call("clipboardGet", json!({}))).and_then(|v| v.get("text")?.as_str().map(str::to_owned));
    system.or_else(|| Some(own.borrow().clone())).filter(|t| !t.is_empty())
}

/// egui as eframe sets it up on a phone or tablet: one window, so viewports are embedded, and up to two
/// passes when egui asks to discard one.
fn new_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(true);
    ctx.options_mut(|o| o.max_passes = NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN));
    ctx
}

fn services(clipboard: &Rc<RefCell<String>>, platform: Option<&Rc<dyn Platform>>, files: Option<&Files>, dirs: Option<&Dirs>) -> Services {
    let file_dialog: Box<dyn Fn(_) -> _> = match files {
        // The system's picker, through the documents' mirrors (`files.rs`).
        Some(files) => {
            let dialogs = files.dialogs();
            Box::new(move |req| crate::files::Dialogs::ask(&dialogs, req))
        }
        // No platform: the app's own file browser (an `Err` answer).
        None => Box::new(|_| {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Err("no system file dialog".to_owned()));
            rx
        }),
    };
    let print = platform.zip(dirs).map(|(platform, dirs)| {
        let (platform, dir) = (Rc::clone(platform), dirs.print());
        Box::new(move |pdf: &[u8]| print(platform.as_ref(), &dir, pdf)) as Box<dyn Fn(&[u8]) -> Result<(), String>>
    });
    Services {
        file_dialog: Some(file_dialog),
        clipboard_text: Some(Box::new({
            let (clipboard, platform) = (Rc::clone(clipboard), platform.cloned());
            move || clipboard_text(platform.as_ref(), &clipboard)
        })),
        print,
        ..Services::default()
    }
}

/// File › Print: the PDF goes to Android's print service, which shows its own dialog (printers, Save as
/// PDF). Earlier print files are deleted (the print service reads the file only while its dialog is open).
fn print(platform: &dyn Platform, dir: &Path, pdf: &[u8]) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    let path = dir.join("document.pdf");
    store::write_atomic(&path, pdf).map_err(|e| e.to_string())?;
    match platform.call("print", json!({"path": path.to_string_lossy(), "name": "Folio Writer document"})) {
        Some(v) if v.get("ok").and_then(serde_json::Value::as_bool) == Some(true) => Ok(()),
        _ => Err("printing isn't available".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Action, Pointer, Tool};
    use crate::keymap::{code, meta};
    use wordcraft_doc::StoryRef;

    fn tablet() -> Window {
        // A 10" tablet in landscape: 2560×1600 at 320 dpi, status bar 48 px, navigation bar 96 px.
        Window { width: 2560, height: 1600, native_pixels_per_point: 2.0, insets: [0, 48, 0, 96] }
    }

    /// One frame, its texture changes taken as a renderer would (egui asserts they're never dropped).
    fn step(host: &mut Host) -> Option<Frame> {
        let mut frame = host.frame()?;
        frame.textures.clear();
        Some(frame)
    }

    fn host() -> Host {
        let mut host = Host::new(&Launch { sample: true }, None, None);
        host.set_window(Some(tablet()));
        host.set_max_texture_side(8192);
        host
    }

    #[test]
    fn launch_flags() {
        assert!(Launch::parse("--sample").sample);
        assert!(Launch::parse("  --control 7981 --sample ").sample);
        assert!(!Launch::parse("").sample);
        assert!(!Launch::parse("--samples").sample);
    }

    #[test]
    fn no_window_no_frame() {
        let mut host = Host::new(&Launch::default(), None, None);
        assert!(step(&mut host).is_none());
        host.set_window(Some(tablet()));
        assert!(step(&mut host).is_some());
        host.set_window(None);
        assert!(step(&mut host).is_none());
    }

    #[test]
    fn the_sample_document_renders_inside_the_system_bars() {
        let mut host = host();
        let mut last = None;
        // The app draws nothing in its first two frames.
        for _ in 0..4 {
            last = step(&mut host);
        }
        let frame = last.expect("frame");
        assert!(!frame.primitives.is_empty(), "the window has content");
        let ppp = frame.pixels_per_point;
        assert!(ppp > 0.0);
        let bottom = (1600.0 - 96.0) / ppp;
        for p in &frame.primitives {
            assert!(p.clip_rect.min.y >= 48.0 / ppp - 0.5, "nothing under the status bar: {:?}", p.clip_rect);
            assert!(p.clip_rect.max.y <= bottom + 0.5, "nothing under the navigation bar: {:?}", p.clip_rect);
        }
        assert!(host.app().session.doc.plain_text(StoryRef::Body).len() > 100, "the sample document is open");
    }

    #[test]
    fn typing_reaches_the_document() {
        let mut host = host();
        for _ in 0..4 {
            step(&mut host);
        }
        let before = host.app().session.doc.plain_text(StoryRef::Body);
        for c in ['Q', 'z'] {
            let keycode = code::A + u32::from(c.to_ascii_lowercase()) - u32::from('a');
            let meta_state = if c.is_uppercase() { meta::SHIFT_ON } else { 0 };
            host.key(&KeyPress { keycode, meta_state, pressed: true, typed: Some(c.to_string()) });
            host.key(&KeyPress { keycode, meta_state, pressed: false, typed: None });
            step(&mut host);
        }
        let after = host.app().session.doc.plain_text(StoryRef::Body);
        assert_eq!(after.chars().count(), before.chars().count() + 2, "two characters typed");
        assert!(after.contains("Qz"));
    }

    #[test]
    fn a_new_context_keeps_the_document() {
        let mut host = host();
        for _ in 0..3 {
            step(&mut host);
        }
        host.key(&KeyPress { keycode: code::A + 23, meta_state: 0, pressed: true, typed: Some("x".into()) });
        step(&mut host);
        let text = host.app().session.doc.plain_text(StoryRef::Body);
        host.reset_context();
        assert_eq!(host.app().session.doc.plain_text(StoryRef::Body), text);
        let mut last = None;
        for _ in 0..3 {
            last = step(&mut host);
        }
        assert!(last.is_some_and(|f| !f.primitives.is_empty()), "the new context draws");
        assert!(host.app().session.can_undo(), "the undo history stays");
        assert!(!host.app().ui.show_discord);
    }

    #[test]
    fn a_tap_and_mouse_input_never_panic() {
        let mut host = host();
        for _ in 0..3 {
            step(&mut host);
        }
        let p = [Pointer { id: 0, x: 1280.0, y: 800.0, pressure: 1.0 }];
        for (mouse_source, tool, action) in [
            (false, Tool::Finger, Action::Down),
            (false, Tool::Finger, Action::Up),
            (true, Tool::Mouse, Action::HoverMove),
            (true, Tool::Mouse, Action::Down),
            (true, Tool::Mouse, Action::Up),
            (true, Tool::Mouse, Action::Scroll),
            (true, Tool::Finger, Action::Down),
            (true, Tool::Finger, Action::Move),
            (true, Tool::Finger, Action::Up),
        ] {
            let m = Motion {
                mouse_source,
                tool,
                device_id: 1,
                action,
                index: 0,
                canceled: false,
                no_focus_change: false,
                pointers: &p,
                buttons: 0,
                scroll: egui::vec2(0.0, -1.0),
                meta_state: 0,
            };
            host.motion(&m);
            step(&mut host);
        }
        host.set_focused(false);
        step(&mut host);
    }

    /// Android, as far as the host can tell: records the calls, answers the clipboard.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<(String, serde_json::Value)>>,
        clipboard: RefCell<Option<String>>,
    }

    impl Platform for Fake {
        fn call(&self, op: &str, args: serde_json::Value) -> Option<serde_json::Value> {
            self.calls.borrow_mut().push((op.to_owned(), args.clone()));
            match op {
                "clipboardGet" => self.clipboard.borrow().as_ref().map(|t| json!({"text": t})),
                "clipboardSet" => {
                    *self.clipboard.borrow_mut() = args.get("text").and_then(|t| t.as_str()).map(str::to_owned);
                    None
                }
                "grants" => Some(json!({"uris": []})),
                "print" => Some(json!({"ok": true})),
                _ => None,
            }
        }
    }

    impl Fake {
        fn ops(&self, op: &str) -> Vec<serde_json::Value> {
            self.calls.borrow().iter().filter(|(o, _)| o == op).map(|(_, a)| a.clone()).collect()
        }
    }

    fn android(dirs: Option<Dirs>) -> (Host, Rc<Fake>) {
        let fake = Rc::new(Fake::default());
        let mut host = Host::new(&Launch { sample: true }, dirs, Some(Rc::clone(&fake) as Rc<dyn Platform>));
        host.set_window(Some(tablet()));
        host.set_max_texture_side(8192);
        for _ in 0..3 {
            step(&mut host);
        }
        (host, fake)
    }

    fn press(host: &mut Host, keycode: u32, meta_state: u32) {
        host.sync_modifiers(meta_state);
        host.key(&KeyPress { keycode, meta_state, pressed: true, typed: None });
        host.key(&KeyPress { keycode, meta_state, pressed: false, typed: None });
        step(host);
    }

    #[test]
    fn copy_and_paste_use_the_system_clipboard() {
        let (mut host, fake) = android(None);
        press(&mut host, code::A, meta::CTRL_ON);
        press(&mut host, code::A + 2, meta::CTRL_ON);
        let copied = fake.clipboard.borrow().clone().expect("copied to the system clipboard");
        assert!(copied.len() > 100);
        // Text copied in another app is pasted.
        *fake.clipboard.borrow_mut() = Some("from another app".into());
        press(&mut host, code::A + 21, meta::CTRL_ON);
        step(&mut host);
        assert!(host.app().session.doc.plain_text(StoryRef::Body).contains("from another app"));
    }

    #[test]
    fn back_closes_what_is_open_else_leaves_the_app() {
        let (mut host, fake) = android(None);
        host.app_mut().ui.backstage = true;
        step(&mut host);
        press(&mut host, code::BACK, 0);
        step(&mut host);
        assert!(!host.app().ui.backstage, "Back closed the backstage");
        assert!(fake.ops("moveTaskToBack").is_empty());
        press(&mut host, code::BACK, 0);
        step(&mut host);
        assert_eq!(fake.ops("moveTaskToBack").len(), 1, "nothing open: the app goes to the background");
    }

    #[test]
    fn the_pointer_shape_is_sent_when_it_changes() {
        let (mut host, fake) = android(None);
        let sent = fake.ops("pointerIcon").len();
        assert_eq!(sent, 1, "once at the start");
        for _ in 0..3 {
            step(&mut host);
        }
        assert_eq!(fake.ops("pointerIcon").len(), sent, "not again while it stays");
    }

    #[test]
    fn a_document_sent_to_the_app_opens() {
        let root = crate::store::tests::temp_dir("host-incoming");
        let (mut host, fake) = android(Some(Dirs::new(&root)));
        assert_eq!(fake.ops("grants").len(), 1);
        host.event(Inbound::Incoming(vec![crate::files::Picked {
            uri: "content://x/doc/1".into(),
            name: "Minutes.docx".into(),
            writable: true,
            ..Default::default()
        }]));
        let copy = fake.ops("copyIn").pop().expect("copied in");
        let path = PathBuf::from(copy["path"].as_str().unwrap());
        // Android copies it.
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("text.insert", &json!({"text": "Minutes of the meeting"})).unwrap();
        wordcraft_engine::io::save_path_with(&path, &s.doc, None, &Default::default()).unwrap();
        let req = copy["req"].as_u64().unwrap();
        host.event(platform::parse(&json!({"event": "copiedIn", "req": req, "remote": {"size": 1}}).to_string()).unwrap());
        step(&mut host);
        step(&mut host);
        assert!(host.app().session.doc.plain_text(StoryRef::Body).contains("Minutes of the meeting"));
        assert_eq!(host.app().session.path.as_deref(), Some(path.as_path()));
        // Saving writes it back to Android's document.
        press(&mut host, code::A + 18, meta::CTRL_ON);
        assert!(fake.ops("writeBack").iter().any(|w| w["uri"] == "content://x/doc/1"), "{:?}", fake.calls.borrow());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn printing_hands_a_pdf_to_android() {
        let root = crate::store::tests::temp_dir("host-print");
        let (mut host, fake) = android(Some(Dirs::new(&root)));
        host.app_mut().run("ui.print", json!({})).unwrap();
        let print = fake.ops("print").pop().expect("print");
        let pdf = std::fs::read(print["path"].as_str().unwrap()).unwrap();
        assert!(pdf.starts_with(b"%PDF"));
        let _ = std::fs::remove_dir_all(root);
    }
}
