//! The Android event loop: the activity's window and lifecycle, input, and frames only when egui asks
//! for one (nothing is drawn while idle).
//!
//! Every Android call happens on the `android_main` thread. Calls that take android-activity's write
//! lock (`enable_motion_axis`) are made before the loop: inside `poll_events` they would deadlock.

mod bridge;
mod entry;
mod gfx;
mod keychars;

use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use android_activity::input::{Axis, ImeOptions, InputEvent, InputType, KeyAction, KeyEvent, MotionAction, MotionEvent, TextInputAction, ToolType};
use android_activity::{AndroidApp, AndroidAppWaker, InputStatus, MainEvent, PollEvent};
use ndk::configuration::UiModeNight;
use ndk::native_window::NativeWindow;

use crate::host::{Host, Launch, Window};
use crate::ime;
use crate::input::{Action, FLAG_NO_FOCUS_CHANGE, KeyPress, Motion, Pointer, Tool};
use crate::platform::{self, Platform, inbox};
use crate::store::Dirs;
use gfx::{Backend, Gpu, PaintJob, Painted, WindowSurface};
use keychars::KeyChars;

/// `AINPUT_SOURCE_MOUSE`'s own bit: set for mice and for touchpads moving the mouse pointer.
const SOURCE_MOUSE_BIT: u32 = 0x2000;
/// `AMOTION_EVENT_FLAG_CANCELED`: the pointer going up was withdrawn, not lifted.
const FLAG_CANCELED: u32 = 0x20;
/// Waits before attaching a window again after attaching failed: doubling from the first to the last.
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(8);
/// How long leaving waits for the settings and the recovery snapshot to reach storage.
const FILES_WAIT: Duration = Duration::from_secs(2);
/// Panics in frames this close together end the app (the next start recovers the document).
const PANIC_WINDOW: Duration = Duration::from_secs(60);
const PANICS_MAX: usize = 3;

pub(crate) fn main(app: AndroidApp) {
    init_logging();
    let args = entry::launch_args();
    log::info!("Folio Writer {} starting{}", env!("CARGO_PKG_VERSION"), if args.is_empty() { String::new() } else { format!(" with {args}") });
    // The wheel's axes are off by default.
    app.enable_motion_axis(Axis::Vscroll);
    app.enable_motion_axis(Axis::Hscroll);
    platform::saved::set_running(true);
    let mut state = State::new(app, &args);
    // A panic outside a frame (input, a report from Android, upkeep): the document is still saved below.
    if std::panic::catch_unwind(AssertUnwindSafe(|| state.run())).is_err() {
        log::error!("the event loop panicked: keeping the document for the next start");
    }
    inbox::set_waker(None);
    if state.host.quit_requested() {
        state.host.quit(FILES_WAIT);
    } else {
        state.host.persist();
        if !state.host.wait_for_files(FILES_WAIT) {
            log::warn!("the recovery snapshot wasn't written in time");
        }
    }
    platform::saved::set_running(false);
    log::info!("Folio Writer stopping");
}

fn init_logging() {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("FolioWriter"));
    // Every panic, caught (`Session::run`'s guard) or not, reaches logcat with its backtrace, and without
    // the text it quotes (`crash.rs`). `entry.rs` catches what leaves `android_main`, so android-activity
    // doesn't log the message again unredacted.
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(ToString::to_string);
        log::error!("{}", crate::crash::headline(location.as_deref(), info.payload_as_str()));
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        for line in trace.lines() {
            log::error!("{line}");
        }
    }));
}

/// When egui wants the next frame, from any thread.
struct Repaint {
    at: Mutex<Option<Instant>>,
    waker: AndroidAppWaker,
}

impl Repaint {
    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        self.at.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// egui's repaint callback: runs under egui's lock, so it only records the time and wakes the loop.
    fn request(&self, delay: Duration) {
        let Some(when) = Instant::now().checked_add(delay) else { return };
        let mut at = self.lock();
        if at.is_none_or(|t| when < t) {
            *at = Some(when);
            drop(at);
            self.waker.wake();
        }
    }

    /// Whether a frame is due, forgetting the request if so.
    fn take_due(&self, now: Instant) -> bool {
        let mut at = self.lock();
        let due = at.is_some_and(|t| t <= now);
        if due {
            *at = None;
        }
        due
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.lock().map(|t| t.saturating_duration_since(now))
    }
}

/// android-activity rounds a wait down to whole milliseconds: round up, or a sub-millisecond wait spins.
fn whole_ms(d: Duration) -> Duration {
    Duration::from_millis(u64::try_from(d.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX).min(60_000))
}

struct State {
    // Fields drop in this order: the surface before the device, both before the window.
    surface: Option<WindowSurface>,
    gpu: Option<Gpu>,
    window: Option<NativeWindow>,
    app: AndroidApp,
    host: Host,
    repaint: Arc<Repaint>,
    backend: Backend,
    keys: KeyChars,
    pointers: Vec<Pointer>,
    /// Draw as soon as possible (the window changed or input arrived).
    redraw: bool,
    quit: bool,
    /// When to try attaching the window again, after attaching failed.
    retry: Option<Instant>,
    retry_delay: Duration,
    /// A device existed before: a new one needs a new egui context (egui keeps no texture pixels).
    had_gpu: bool,
    /// Graphics devices lost so far: after two, OpenGL ES instead of Vulkan.
    losses: u32,
    /// Frames skipped in a row: the retries back off.
    skips: u32,
    presented: u32,
    /// When `android_main` started: the first frame's log line says how long it took.
    started: Instant,
    /// When the host's upkeep (the recovery snapshot) is next due.
    upkeep: Option<Instant>,
    /// When frames panicked lately.
    panics: Vec<Instant>,
    /// The keyboard's settings last sent.
    ime_editor: Option<ime::Editor>,
    /// Debug builds: `--panic-test` panics after the first frames, so CI sees a panic reach logcat.
    #[cfg(debug_assertions)]
    panic_test: bool,
}

impl State {
    fn new(app: AndroidApp, args: &str) -> Self {
        let repaint = Arc::new(Repaint { at: Mutex::new(None), waker: app.create_waker() });
        // Kotlin's reports wake the loop (and any that came before it are picked up).
        let waker = app.create_waker();
        inbox::set_waker(Some(Arc::new(move || waker.wake())));
        let platform: Rc<dyn Platform> = Rc::new(bridge::AndroidPlatform);
        let mut state = Self {
            surface: None,
            gpu: None,
            window: None,
            host: Host::new(&Launch::parse(args), app.internal_data_path().map(|p| Dirs::new(&p)), Some(platform)),
            repaint,
            backend: Backend::parse(args),
            keys: KeyChars::default(),
            pointers: Vec::with_capacity(8),
            redraw: true,
            quit: false,
            retry: None,
            retry_delay: RETRY_FIRST,
            had_gpu: false,
            losses: 0,
            skips: 0,
            presented: 0,
            started: Instant::now(),
            upkeep: None,
            panics: Vec::new(),
            ime_editor: None,
            #[cfg(debug_assertions)]
            panic_test: args.split_whitespace().any(|a| a == "--panic-test"),
            app,
        };
        state.install_repaint_callback();
        state.read_config();
        let sdk = state.app.config().sdk_version();
        state.host.set_legacy_touchpad((31..=33).contains(&sdk));
        state
    }

    fn install_repaint_callback(&self) {
        let repaint = Arc::clone(&self.repaint);
        self.host.ctx().set_request_repaint_callback(move |info| repaint.request(info.delay));
    }

    fn run(&mut self) {
        while !self.quit {
            let app = self.app.clone();
            app.poll_events(self.timeout(), |event| self.on_event(event));
            // Without draining every time, no further input arrives.
            // The keyboard's states before keys and taps, which commit a composition the states describe.
            self.drain_inbox();
            self.drain_input();
            if self.quit {
                break;
            }
            let now = Instant::now();
            if self.surface.is_none() && self.window.is_some() && self.retry.is_some_and(|t| t <= now) {
                self.attach();
            }
            let due = self.repaint.take_due(now);
            let mut framed = false;
            if self.surface.is_some() && (self.redraw || due || self.host.has_events()) {
                self.redraw = false;
                self.frame();
                framed = true;
            }
            if framed {
                self.ime_requests();
            }
            let now = Instant::now();
            if framed || self.upkeep.is_some_and(|t| t <= now) {
                self.upkeep = self.host.maintain(now);
            }
            if self.quit {
                break;
            }
            // Returning from `android_main` finishes the activity, once documents are written back.
            if self.host.quit_requested() {
                if self.host.ready_to_quit() {
                    break;
                }
                // Looks again when a write-back reports, or soon.
                self.repaint.request(Duration::from_millis(250));
            }
        }
    }

    /// How long to wait for events: not at all when a frame is due, else until egui's next repaint (with a
    /// surface) or the next attempt to attach the window (without), or the host's upkeep, whichever
    /// comes first. With none of these, indefinitely.
    fn timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        if self.surface.is_some() && (self.redraw || self.host.has_events()) {
            return Some(Duration::ZERO);
        }
        let next = if self.surface.is_some() { self.repaint.wait(now) } else { self.retry.map(|t| t.saturating_duration_since(now)) };
        let upkeep = self.upkeep.map(|t| t.saturating_duration_since(now));
        next.into_iter().chain(upkeep).min().map(whole_ms)
    }

    fn on_event(&mut self, event: PollEvent<'_>) {
        let PollEvent::Main(event) = event else { return };
        match event {
            MainEvent::InitWindow { .. } => {
                self.retry_delay = RETRY_FIRST;
                self.attach();
            }
            // The window goes once this returns: the surface must go first.
            MainEvent::TerminateWindow { .. } => self.detach(),
            MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } | MainEvent::RedrawNeeded { .. } => {
                self.update_window();
                self.redraw = true;
            }
            MainEvent::GainedFocus => {
                self.host.set_focused(true);
                self.redraw = true;
            }
            MainEvent::LostFocus => {
                self.host.set_focused(false);
                self.keys.clear();
                self.redraw = true;
            }
            MainEvent::ConfigChanged { .. } => {
                self.read_config();
                self.update_window();
                self.redraw = true;
            }
            MainEvent::LowMemory => log::info!("the system is low on memory"),
            // The app may be killed from here on without another word.
            MainEvent::Pause => self.host.persist(),
            // `onStop` waits for this (`platform::saved`).
            MainEvent::Stop => {
                self.host.persist();
                self.host.wait_for_files(Duration::from_millis(1400));
                platform::saved::done();
            }
            MainEvent::Destroy => self.quit = true,
            _ => {}
        }
    }

    /// The system's dark mode (density is read with the window size).
    fn read_config(&mut self) {
        let theme = match self.app.config().ui_mode_night() {
            UiModeNight::Yes => Some(egui::Theme::Dark),
            UiModeNight::No => Some(egui::Theme::Light),
            _ => None,
        };
        self.host.set_system_theme(theme);
    }

    /// Draws to the activity's window: a surface for it, and a device if there's none. On failure, tries
    /// again later, waiting longer each time.
    fn attach(&mut self) {
        // One surface per window: connecting a window twice fails.
        self.drop_surface();
        let Some(window) = self.app.native_window() else {
            self.window = None;
            self.retry = None;
            return;
        };
        let surface = match &self.gpu {
            Some(gpu) => gpu.attach(&window),
            None => Gpu::new(&window, self.backend).map(|(gpu, surface)| {
                if self.had_gpu {
                    self.host.reset_context();
                    self.install_repaint_callback();
                }
                self.had_gpu = true;
                self.host.set_max_texture_side(gpu.max_texture_side());
                self.gpu = Some(gpu);
                surface
            }),
        };
        match surface {
            Ok(surface) => {
                self.surface = Some(surface);
                self.retry = None;
                self.retry_delay = RETRY_FIRST;
            }
            Err(e) => {
                log::error!("can't draw to the window ({e}); trying again in {} ms", self.retry_delay.as_millis());
                self.retry = Instant::now().checked_add(self.retry_delay);
                self.retry_delay = self.retry_delay.saturating_mul(2).min(RETRY_MAX);
            }
        }
        self.window = Some(window);
        self.update_window();
        self.redraw = true;
    }

    fn detach(&mut self) {
        self.drop_surface();
        self.window = None;
        self.retry = None;
        self.host.set_window(None);
    }

    /// Drops the surface. A panic here (in a driver's teardown) must not leave `android_main`: in the
    /// `TerminateWindow` callback, Android would then wait for the window forever. The device goes too,
    /// so the next window starts afresh.
    fn drop_surface(&mut self) {
        if !gfx::drop_guarded(self.surface.take()) {
            log::error!("releasing the window's surface panicked; starting the graphics device again");
            gfx::drop_guarded(self.gpu.take());
        }
    }

    fn update_window(&mut self) {
        let Some(window) = &self.window else { return };
        let Some([width, height]) = gfx::window_size(window) else { return };
        // The activity keeps the surface clear of the system bars, cutouts and keyboard: no insets here.
        let density = self.app.config().density().unwrap_or(160);
        let window = Window { width, height, native_pixels_per_point: density as f32 / 160.0, insets: [0; 4] };
        if self.host.window() != Some(window) {
            log::info!("window {width}×{height} px at {density} dpi");
        }
        self.host.set_window(Some(window));
    }

    fn frame(&mut self) {
        // A panic in the app's interface code: the document survives, the interface starts again.
        let frame = match std::panic::catch_unwind(AssertUnwindSafe(|| self.host.frame())) {
            Ok(frame) => frame,
            Err(_) => return self.after_panic(),
        };
        let Some(mut frame) = frame else { return };
        let Some(gpu) = self.gpu.as_mut() else {
            // Textures no renderer saw: the next device needs a new egui context.
            frame.textures.clear();
            self.had_gpu = true;
            return;
        };
        let clear = self.host.clear_color();
        let mut job = PaintJob { primitives: &frame.primitives, textures: &mut frame.textures, pixels_per_point: frame.pixels_per_point, clear };
        match gpu.paint(self.surface.as_mut(), self.window.as_ref(), &mut job) {
            Painted::Presented => {
                if self.presented == 0 {
                    log::info!("first frame after {} ms", self.started.elapsed().as_millis());
                }
                self.presented = self.presented.saturating_add(1);
                self.skips = 0;
            }
            // Try again at the next vsync, backing off to a second while it keeps failing.
            Painted::Skipped => {
                self.repaint.request(Duration::from_millis(16 << self.skips.min(6)));
                self.skips = self.skips.saturating_add(1);
            }
            Painted::RecreateSurface => self.attach(),
            Painted::Reinit => {
                self.losses = self.losses.saturating_add(1);
                if self.losses >= 2 && self.backend == Backend::Auto {
                    log::warn!("the graphics device was lost {} times: using OpenGL ES", self.losses);
                    self.backend = Backend::Gl;
                }
                self.drop_surface();
                gfx::drop_guarded(self.gpu.take());
                if self.losses == 1 {
                    self.attach();
                } else {
                    self.retry = Instant::now().checked_add(self.retry_delay);
                    self.retry_delay = self.retry_delay.saturating_mul(2).min(RETRY_MAX);
                }
            }
        }
        #[cfg(debug_assertions)]
        if self.panic_test && self.presented >= 3 {
            panic_test();
        }
    }

    /// After a frame panicked (the hook logged it): a snapshot of the document first, then a new egui
    /// context and graphics device, as after a lost device. Panics in a row end the app.
    fn after_panic(&mut self) {
        self.host.persist();
        let now = Instant::now();
        self.panics.retain(|t| now.duration_since(*t) < PANIC_WINDOW);
        self.panics.push(now);
        if self.panics.len() >= PANICS_MAX {
            log::error!("the interface panicked {} times in a minute: closing (the document is kept for the next start)", self.panics.len());
            self.quit = true;
            return;
        }
        log::error!("the interface panicked: starting it again");
        self.drop_surface();
        gfx::drop_guarded(self.gpu.take());
        self.had_gpu = true;
        self.attach();
    }

    /// The on-screen keyboard: shown and hidden through GameActivity, its text set through the activity
    /// (which tags the keyboard's states with the push they follow). Never inside `poll_events`.
    fn ime_requests(&mut self) {
        for request in self.host.take_ime_requests() {
            match request {
                ime::Request::Show => {
                    self.ime_editor_info();
                    self.app.show_soft_input(false);
                }
                ime::Request::Hide => self.app.hide_soft_input(false),
                ime::Request::Push { text, anchor, focus, generation } => {
                    self.ime_editor_info();
                    let args = serde_json::json!({"text": text, "anchor": anchor, "focus": focus, "generation": generation});
                    bridge::AndroidPlatform.call("imePush", args);
                }
            }
        }
    }

    /// A multi-line editor with suggestions and sentence capitals, Enter typing a new line, never
    /// full-screen; for a password, no suggestions and no learning; for an equation, no capitals and no
    /// corrections.
    fn ime_editor_info(&mut self) {
        let editor = self.host.ime_editor();
        if self.ime_editor == Some(editor) {
            return;
        }
        self.ime_editor = Some(editor);
        let fixed = ImeOptions::IME_FLAG_NO_FULLSCREEN | ImeOptions::IMG_FLAG_NO_EXTRACT_UI;
        let (input_type, options) = match editor {
            ime::Editor::Password => (
                InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_VARIATION_PASSWORD | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS,
                fixed | ImeOptions::IME_FLAG_NO_PERSONALIZED_LEARNING,
            ),
            ime::Editor::Raw => (
                InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_VARIATION_VISIBLE_PASSWORD | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS,
                fixed | ImeOptions::IME_FLAG_NO_ENTER_ACTION,
            ),
            ime::Editor::Text => (
                InputType::TYPE_CLASS_TEXT
                    | InputType::TYPE_TEXT_FLAG_MULTI_LINE
                    | InputType::TYPE_TEXT_FLAG_CAP_SENTENCES
                    | InputType::TYPE_TEXT_FLAG_AUTO_CORRECT,
                fixed | ImeOptions::IME_FLAG_NO_ENTER_ACTION,
            ),
        };
        self.app.set_ime_editor_info(input_type, TextInputAction::None, options);
    }

    /// Kotlin's reports (pickers, copies, write-backs, documents sent to the app).
    fn drain_inbox(&mut self) {
        let (reports, dropped) = inbox::take();
        if dropped > 0 {
            log::warn!("{dropped} reports from Android were dropped");
        }
        for report in reports {
            match platform::parse(&report) {
                Some(inbound) => {
                    self.host.event(inbound);
                    self.redraw = true;
                }
                None => log::warn!("a report from Android this version doesn't know"),
            }
        }
    }

    fn drain_input(&mut self) {
        let app = self.app.clone();
        let mut events = match app.input_events_iter() {
            Ok(events) => events,
            Err(e) => {
                log::warn!("input: {e}");
                return;
            }
        };
        // Input without a surface to draw on would act on a screen the user can't see: it's dropped.
        let live = self.surface.is_some();
        // GameActivity hands over a batch's key events before its motion events: the newest event's
        // modifiers win at the end.
        let mut newest: Option<(i64, u32)> = None;
        let mut seen = |time: i64, meta: u32| {
            if newest.is_none_or(|(t, _)| time >= t) {
                newest = Some((time, meta));
            }
        };
        // GameActivity hands every event to the app whatever this returns.
        while events.next(|event| {
            match event {
                InputEvent::KeyEvent(k) if live => {
                    seen(k.event_time(), k.meta_state().0);
                    self.on_key(k);
                }
                InputEvent::MotionEvent(m) if live => {
                    seen(m.event_time(), m.meta_state().0);
                    self.on_motion(m);
                }
                InputEvent::TextAction(_) if live => self.host.ime_action(),
                // The keyboard's states come through `MainActivity.stateChanged` (the inbox) instead:
                // GameTextInput's own copy is cut at 64 KiB.
                _ => {}
            }
            InputStatus::Handled
        }) {}
        if let Some((_, meta)) = newest {
            self.host.sync_modifiers(meta);
        }
    }

    fn on_key(&mut self, k: &KeyEvent<'_>) {
        let pressed = match k.action() {
            KeyAction::Down => true,
            KeyAction::Up => false,
            _ => return,
        };
        let keycode = u32::from(k.key_code());
        let meta_state = k.meta_state().0;
        let typed = if pressed { self.keys.typed(k.device_id(), keycode, meta_state) } else { None };
        self.host.key(&KeyPress { keycode, meta_state, pressed, typed });
    }

    fn on_motion(&mut self, m: &MotionEvent<'_>) {
        let action = match m.action() {
            MotionAction::Down => Action::Down,
            MotionAction::Up => Action::Up,
            MotionAction::Move => Action::Move,
            MotionAction::Cancel => Action::Cancel,
            MotionAction::PointerDown => Action::PointerDown,
            MotionAction::PointerUp => Action::PointerUp,
            MotionAction::HoverEnter => Action::HoverEnter,
            MotionAction::HoverMove => Action::HoverMove,
            MotionAction::HoverExit => Action::HoverExit,
            MotionAction::Scroll => Action::Scroll,
            MotionAction::ButtonPress => Action::ButtonPress,
            MotionAction::ButtonRelease => Action::ButtonRelease,
            _ => Action::Other,
        };
        self.pointers.clear();
        let mut tool = Tool::Other;
        let mut scroll = egui::Vec2::ZERO;
        for (i, p) in m.pointers().enumerate() {
            if i == 0 {
                tool = match p.tool_type() {
                    ToolType::Finger => Tool::Finger,
                    ToolType::Stylus | ToolType::Eraser => Tool::Stylus,
                    ToolType::Mouse => Tool::Mouse,
                    _ => Tool::Other,
                };
                if action == Action::Scroll {
                    scroll = egui::vec2(p.axis_value(Axis::Hscroll), p.axis_value(Axis::Vscroll));
                }
            }
            self.pointers.push(Pointer { id: p.pointer_id(), x: p.x(), y: p.y(), pressure: p.pressure() });
        }
        let motion = Motion {
            mouse_source: u32::from(m.source()) & SOURCE_MOUSE_BIT != 0,
            tool,
            device_id: m.device_id(),
            action,
            index: m.pointer_index(),
            canceled: m.flags().0 & FLAG_CANCELED != 0,
            no_focus_change: m.flags().0 & FLAG_NO_FOCUS_CHANGE != 0,
            pointers: &self.pointers,
            buttons: m.button_state().0,
            scroll,
            meta_state: m.meta_state().0,
        };
        self.host.motion(&motion);
    }
}

/// Debug builds only, from `--panic-test`: proves in CI that a panic reaches logcat with a backtrace that
/// names functions. The one deliberate panic outside tests; release builds don't contain it.
#[cfg(debug_assertions)]
#[allow(clippy::panic)]
#[inline(never)]
#[cold]
fn panic_test() {
    panic!("--panic-test: a deliberate panic");
}
