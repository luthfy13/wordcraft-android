//! The platform-independent half of the Android host.
//!
//! [`Host`] owns egui's context and the WordCraft app, turns input into egui events and runs a frame the
//! way eframe does: `raw_input_hook`, then `logic` and `ui` in one pass. It knows nothing about Android
//! or the GPU, so tests drive it on the host.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::time::Instant;

use egui::output::OutputCommand;
use egui::{ClippedPrimitive, Event, RawInput, Rect, TexturesDelta, Theme, ViewportId, ViewportInfo, pos2};
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Services, WordApp};

use crate::input::{KeyPress, Motion, Translator};
use crate::recovery::Recovery;
use crate::store::{self, Dirs};

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
    /// Links the user opened.
    pub urls: Vec<String>,
}

/// The WordCraft app and egui, driven by the platform.
pub struct Host {
    ctx: egui::Context,
    app: WordApp,
    input: Translator,
    events: Vec<Event>,
    /// Text copied in the app. The system clipboard comes in a later phase; until then copy and paste
    /// work inside the app.
    clipboard: Rc<RefCell<String>>,
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
    pub fn new(launch: &Launch, dirs: Option<Dirs>) -> Self {
        let clipboard = Rc::new(RefCell::new(String::new()));
        // Added fonts before the first layout, as on the desktop.
        if let Some(dirs) = &dirs {
            store::load_fonts_and_dictionaries(dirs);
        }
        let doc = if launch.sample { wordcraft_engine::sample::sample_document() } else { wordcraft_doc::Document::new() };
        let mut app = WordApp::new(Session::new(doc), services(&clipboard));
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
        Self {
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
        }
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
        self.save_prefs();
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
        let mut app = WordApp::new(session, services(&self.clipboard));
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
        let clipboard = &self.clipboard;
        self.input.key(k, || Some(clipboard.borrow().clone()), &mut self.events);
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
        self.app.raw_input_hook(&mut raw);
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        });
        let mut urls = Vec::new();
        for command in out.platform_output.commands {
            match command {
                OutputCommand::CopyText(text) => *self.clipboard.borrow_mut() = text,
                OutputCommand::OpenUrl(url) => urls.push(url.url),
                OutputCommand::CopyImage(_) => {}
            }
        }
        let primitives = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        Some(Frame { primitives, textures: out.textures_delta, pixels_per_point: out.pixels_per_point, urls })
    }

    /// Whether the app asked to quit (File › Close on the last document, or Exit).
    pub fn quit_requested(&self) -> bool {
        self.app.quit_requested
    }
}

/// egui as eframe sets it up on a phone or tablet: one window, so viewports are embedded, and up to two
/// passes when egui asks to discard one.
fn new_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(true);
    ctx.options_mut(|o| o.max_passes = NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN));
    ctx
}

fn services(clipboard: &Rc<RefCell<String>>) -> Services {
    Services {
        // No system file dialog yet: the app's own file browser opens instead (an `Err` answer).
        file_dialog: Some(Box::new(|_| {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Err("Android has no file dialog in this version".to_owned()));
            rx
        })),
        clipboard_text: Some(Box::new({
            let clipboard = Rc::clone(clipboard);
            move || Some(clipboard.borrow().clone()).filter(|t| !t.is_empty())
        })),
        ..Services::default()
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
        let mut host = Host::new(&Launch { sample: true }, None);
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
        let mut host = Host::new(&Launch::default(), None);
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
}
