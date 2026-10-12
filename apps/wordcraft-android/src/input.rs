//! Android input → egui events.
//!
//! Touch follows egui-winit: every finger is an `Event::Touch` (egui's pinch zoom), and the first finger
//! also drives the pointer (press, move, release, then `PointerGone`). Mice and touchpads drive the
//! pointer directly: hover, every button (from the event's button state, so press/release order doesn't
//! matter) and the wheel. A touchpad's two-finger gesture arrives as synthetic fingers from the mouse with
//! no button down: it scrolls, a pinch zooms, and a tap without movement clicks. A gesture Android cancels
//! (a palm, say) releases without counting as a click. Keys follow egui-winit too: Cut/Copy/Paste
//! shortcuts become clipboard events, other keys become `Event::Key`, printable text becomes `Event::Text`.
//!
//! Pure data, so it builds and is tested on the host; `android/mod.rs` fills [`Motion`] and [`KeyPress`]
//! from `android_activity`'s events.

use egui::{Event, Key, Modifiers, PointerButton, Pos2, TouchDeviceId, TouchId, TouchPhase, Vec2, pos2, vec2};

use crate::keymap;

/// `AMOTION_EVENT_BUTTON_*` bits of a motion event's button state.
pub mod button {
    pub const PRIMARY: u32 = 0x1;
    pub const SECONDARY: u32 = 0x2;
    pub const TERTIARY: u32 = 0x4;
    pub const BACK: u32 = 0x8;
    pub const FORWARD: u32 = 0x10;
}

const BUTTONS: [(u32, PointerButton); 5] = [
    (button::PRIMARY, PointerButton::Primary),
    (button::SECONDARY, PointerButton::Secondary),
    (button::TERTIARY, PointerButton::Middle),
    (button::BACK, PointerButton::Extra1),
    (button::FORWARD, PointerButton::Extra2),
];

/// How far (points) a cancelled gesture's pointer is nudged and brought back before its release: further
/// than egui's click distance (6 points), so the release is never a click.
const CANCEL_NUDGE: f32 = 16.0;

/// How far (points) a touchpad's two fingers may drift and still make a tap (a click).
const TAP_SLOP: f32 = 8.0;

/// `AMOTION_EVENT_FLAG_NO_FOCUS_CHANGE`: on Android 12–13 touchpads, set on two-finger gestures and never on
/// a tap or tap-drag, which arrive in the same shape (one finger, no button).
pub const FLAG_NO_FOCUS_CHANGE: u32 = 0x40;

/// The tool of a motion event's first pointer (`AMOTION_EVENT_TOOL_TYPE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Finger,
    Stylus,
    Mouse,
    Other,
}

/// A motion event's action (`AMOTION_EVENT_ACTION_*`), with the pointer index for `PointerDown`/`PointerUp`
/// in [`Motion::index`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    Move,
    Cancel,
    PointerDown,
    PointerUp,
    HoverEnter,
    HoverMove,
    HoverExit,
    Scroll,
    ButtonPress,
    ButtonRelease,
    /// Outside, and anything newer: nothing to do.
    Other,
}

/// One pointer of a motion event, in physical pixels from the window's top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// A motion event, independent of `android_activity`.
#[derive(Clone, Copy, Debug)]
pub struct Motion<'a> {
    /// From a mouse or a touchpad moving the mouse pointer (`AINPUT_SOURCE_MOUSE`), not the screen.
    pub mouse_source: bool,
    pub tool: Tool,
    pub device_id: i32,
    pub action: Action,
    /// The pointer that went down or up, for `PointerDown`/`PointerUp`.
    pub index: usize,
    /// `FLAG_CANCELED` on an up: Android withdrew the pointer (a palm, an accidental touch).
    pub canceled: bool,
    /// [`FLAG_NO_FOCUS_CHANGE`].
    pub no_focus_change: bool,
    pub pointers: &'a [Pointer],
    /// `AMOTION_EVENT_BUTTON_*` bits held after this event.
    pub buttons: u32,
    /// `AXIS_HSCROLL`/`AXIS_VSCROLL` for `Scroll` (positive: right and up).
    pub scroll: Vec2,
    pub meta_state: u32,
}

/// A key event, independent of `android_activity`.
#[derive(Clone, Debug)]
pub struct KeyPress {
    pub keycode: u32,
    pub meta_state: u32,
    pub pressed: bool,
    /// The text the key types with this meta state (a pending dead key's accent may come first).
    pub typed: Option<String>,
}

/// A touchpad's two-finger gesture in progress, in pixels.
#[derive(Clone, Copy, Debug)]
struct Swipe {
    device_id: i32,
    start: Pos2,
    /// The fingers' centre at the last event.
    last: Pos2,
    /// The distance between the first two fingers at the last event, for a pinch.
    spread: Option<f32>,
    fingers: usize,
    moved: bool,
}

/// Turns Android input into egui events, keeping the state that needs (which finger drives the pointer,
/// which mouse buttons are down, a touchpad gesture, the modifiers).
#[derive(Debug, Default)]
pub struct Translator {
    /// The finger (device, pointer id) that drives the pointer, as in egui-winit.
    touch_pointer: Option<(i32, i32)>,
    /// Mouse buttons egui was told are down.
    buttons: u32,
    swipe: Option<Swipe>,
    /// Android 12–13, whose touchpads send a tap-drag without a button: tells it from a two-finger swipe
    /// by [`FLAG_NO_FOCUS_CHANGE`].
    legacy_touchpad: bool,
    /// The touchpad (device) whose tap-drag is going on: it holds the primary button.
    tap_drag: Option<i32>,
    /// Where egui last saw the pointer, in points (`None` after `PointerGone`).
    pointer: Option<Pos2>,
    modifiers: Modifiers,
}

impl Translator {
    /// Appends the egui events for a motion event. `pixels_per_point` converts the event's pixels to points.
    pub fn motion(&mut self, m: &Motion<'_>, pixels_per_point: f32, out: &mut Vec<Event>) {
        self.set_modifiers(keymap::modifiers(m.meta_state), out);
        let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
        let in_swipe = self.swipe.is_some_and(|s| s.device_id == m.device_id);
        let swipe_starts = m.mouse_source && m.tool == Tool::Finger && m.action == Action::Down && m.buttons == 0;
        if swipe_starts && self.legacy_touchpad && !m.no_focus_change && self.swipe.is_none() {
            self.tap_drag = Some(m.device_id);
        }
        if self.tap_drag == Some(m.device_id) {
            // The press lasts until the finger lifts: a tap clicks, a tap-drag selects.
            let ends = matches!(m.action, Action::Up | Action::Cancel);
            if ends {
                self.tap_drag = None;
            }
            let held = Motion { buttons: if ends { m.buttons } else { m.buttons | button::PRIMARY }, ..*m };
            self.mouse(&held, ppp, out);
        } else if in_swipe || (swipe_starts && self.swipe.is_none()) {
            self.swipe(m, ppp, out);
        } else if m.mouse_source || m.tool == Tool::Mouse {
            self.mouse(m, ppp, out);
        } else {
            self.touch(m, ppp, out);
        }
    }

    /// Android 12–13 (API 31–33): see [`Self::legacy_touchpad`].
    pub fn set_legacy_touchpad(&mut self, legacy: bool) {
        self.legacy_touchpad = legacy;
    }

    /// Keeps egui's modifiers on the newest event's: GameActivity hands over a batch's key events before its
    /// motion events, so the last event handled isn't always the newest.
    pub fn sync_modifiers(&mut self, meta_state: u32, out: &mut Vec<Event>) {
        self.set_modifiers(keymap::modifiers(meta_state), out);
    }

    /// A touchpad's two-finger gesture. One centre moving scrolls by how far it moved, like a finger on the
    /// screen (Android already applied the user's scrolling direction); fingers spreading or closing zoom;
    /// fingers lifted where they landed click (tap to click).
    fn swipe(&mut self, m: &Motion<'_>, ppp: f32, out: &mut Vec<Event>) {
        let fingers: Vec<Pos2> = m.pointers.iter().map(|p| pos2(p.x, p.y)).filter(|p| p.is_finite()).collect();
        let centre = (!fingers.is_empty()).then(|| fingers.iter().fold(Pos2::ZERO, |a, p| a + p.to_vec2()) / fingers.len() as f32);
        let spread = match fingers.as_slice() {
            [a, b, ..] => Some(a.distance(*b)),
            _ => None,
        };
        let wheel =
            |delta: Vec2, phase: TouchPhase, modifiers: Modifiers| Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, phase, modifiers };
        match m.action {
            Action::Down => {
                let Some(centre) = centre else { return };
                self.swipe = Some(Swipe { device_id: m.device_id, start: centre, last: centre, spread, fingers: fingers.len(), moved: false });
                // The wheel and zoom go to what's under the pointer.
                self.moved(centre / ppp, out);
                out.push(wheel(Vec2::ZERO, TouchPhase::Start, self.modifiers));
            }
            Action::Up | Action::Cancel => {
                let Some(s) = self.swipe.take() else { return };
                out.push(wheel(Vec2::ZERO, TouchPhase::End, self.modifiers));
                if m.action == Action::Up && !m.canceled && !s.moved {
                    let pos = s.start / ppp;
                    self.moved(pos, out);
                    for pressed in [true, false] {
                        out.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: self.modifiers });
                    }
                }
            }
            _ => {
                let modifiers = self.modifiers;
                let (Some(s), Some(centre)) = (self.swipe.as_mut(), centre) else { return };
                if fingers.len() == s.fingers {
                    if let (Some(before), Some(now)) = (s.spread, spread) {
                        let zoom = now / before;
                        if before > 1.0 && zoom.is_finite() && (zoom - 1.0).abs() > 1e-3 {
                            out.push(Event::Zoom(zoom));
                            s.moved = true;
                        }
                    }
                    let delta = (centre - s.last) / ppp;
                    if delta != Vec2::ZERO && delta.is_finite() {
                        out.push(wheel(delta, TouchPhase::Move, modifiers));
                    }
                }
                // A finger landing or lifting moves the centre: no scroll for that.
                s.last = centre;
                s.spread = spread;
                s.fingers = fingers.len();
                s.moved |= (centre - s.start).length() / ppp > TAP_SLOP;
            }
        }
    }

    fn touch(&mut self, m: &Motion<'_>, ppp: f32, out: &mut Vec<Event>) {
        let device = m.device_id;
        let device_id = TouchDeviceId(u64::from(device.unsigned_abs()));
        let at = |p: &Pointer| pos2(p.x / ppp, p.y / ppp);
        let one = |t: &mut Self, p: &Pointer, phase: TouchPhase, out: &mut Vec<Event>| {
            let pos = at(p);
            let force = (p.pressure.is_finite() && p.pressure > 0.0).then_some(p.pressure.min(1.0));
            out.push(Event::Touch { device_id, id: TouchId(u64::from(p.id.unsigned_abs())), phase, pos, force });
            let drives = t.touch_pointer == Some((device, p.id));
            match phase {
                TouchPhase::Start if t.touch_pointer.is_none() => {
                    t.touch_pointer = Some((device, p.id));
                    t.moved(pos, out);
                    out.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: t.modifiers });
                }
                TouchPhase::Move if drives => t.moved(pos, out),
                TouchPhase::End if drives => {
                    t.touch_pointer = None;
                    out.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: t.modifiers });
                    t.gone(out);
                }
                TouchPhase::Cancel if drives => {
                    t.touch_pointer = None;
                    t.release_without_click(pos, &[PointerButton::Primary], out);
                    t.gone(out);
                }
                _ => {}
            }
        };
        match m.action {
            Action::Down | Action::PointerDown => {
                if let Some(p) = m.pointers.get(m.index) {
                    one(self, p, TouchPhase::Start, out);
                }
            }
            Action::Up | Action::PointerUp => {
                if let Some(p) = m.pointers.get(m.index) {
                    one(self, p, if m.canceled { TouchPhase::Cancel } else { TouchPhase::End }, out);
                }
            }
            Action::Move => {
                for p in m.pointers {
                    one(self, p, TouchPhase::Move, out);
                }
            }
            Action::Cancel => {
                for p in m.pointers {
                    one(self, p, TouchPhase::Cancel, out);
                }
            }
            // A stylus hovering moves the pointer without pressing.
            Action::HoverEnter | Action::HoverMove => {
                if self.touch_pointer.is_none()
                    && let Some(p) = m.pointers.first()
                {
                    self.moved(at(p), out);
                }
            }
            Action::HoverExit => {
                if self.touch_pointer.is_none() && self.buttons == 0 && self.pointer.is_some() {
                    self.gone(out);
                }
            }
            Action::Scroll | Action::ButtonPress | Action::ButtonRelease | Action::Other => {}
        }
    }

    fn mouse(&mut self, m: &Motion<'_>, ppp: f32, out: &mut Vec<Event>) {
        let Some(pos) = m.pointers.get(m.index).or(m.pointers.first()).map(|p| pos2(p.x / ppp, p.y / ppp)) else { return };
        match m.action {
            Action::Scroll => {
                // Android: positive is right and up; egui (like winit): positive moves the content right and down.
                let delta = vec2(-m.scroll.x, m.scroll.y);
                if delta != Vec2::ZERO && delta.is_finite() {
                    // The wheel goes to what's under the pointer, even right after a finger lifted.
                    if self.touch_pointer.is_none() {
                        self.moved(pos, out);
                    }
                    out.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, phase: TouchPhase::Move, modifiers: self.modifiers });
                }
            }
            Action::Cancel => self.cancel_buttons(pos, out),
            Action::Up | Action::PointerUp if m.canceled => self.cancel_buttons(pos, out),
            // The mouse left the window (a touchpad also leaves before each click and gesture, then comes back).
            Action::HoverExit => {
                if self.buttons == 0 && self.touch_pointer.is_none() && self.pointer.is_some() {
                    self.gone(out);
                }
            }
            Action::Down
            | Action::PointerDown
            | Action::Up
            | Action::PointerUp
            | Action::Move
            | Action::HoverEnter
            | Action::HoverMove
            | Action::ButtonPress
            | Action::ButtonRelease => {
                self.moved(pos, out);
                self.set_buttons(m.buttons, pos, out);
            }
            Action::Other => {}
        }
    }

    fn cancel_buttons(&mut self, pos: Pos2, out: &mut Vec<Event>) {
        let held: Vec<PointerButton> = BUTTONS.iter().filter(|(bit, _)| self.buttons & bit != 0).map(|&(_, b)| b).collect();
        self.release_without_click(pos, &held, out);
        self.buttons = 0;
        self.gone(out);
    }

    /// Presses and releases egui buttons so they match `held`.
    fn set_buttons(&mut self, held: u32, pos: Pos2, out: &mut Vec<Event>) {
        for (bit, button) in BUTTONS {
            let was = self.buttons & bit != 0;
            let is = held & bit != 0;
            if was != is {
                out.push(Event::PointerButton { pos, button, pressed: is, modifiers: self.modifiers });
            }
        }
        self.buttons = held & BUTTONS.iter().fold(0, |all, (bit, _)| all | bit);
    }

    /// Releases `buttons` at `pos` so that egui doesn't count a click: the pointer first moves further than a
    /// click allows and comes back, so it ends where it was.
    fn release_without_click(&mut self, pos: Pos2, buttons: &[PointerButton], out: &mut Vec<Event>) {
        if buttons.is_empty() {
            return;
        }
        self.moved(pos + vec2(CANCEL_NUDGE, 0.0), out);
        self.moved(pos, out);
        for &button in buttons {
            out.push(Event::PointerButton { pos, button, pressed: false, modifiers: self.modifiers });
        }
    }

    fn moved(&mut self, pos: Pos2, out: &mut Vec<Event>) {
        if pos.is_finite() {
            self.pointer = Some(pos);
            out.push(Event::PointerMoved(pos));
        }
    }

    fn gone(&mut self, out: &mut Vec<Event>) {
        self.pointer = None;
        out.push(Event::PointerGone);
    }

    /// Appends the egui events for a key event. `paste` reads the clipboard for a paste shortcut.
    pub fn key(&mut self, k: &KeyPress, paste: impl FnOnce() -> Option<String>, out: &mut Vec<Event>) {
        self.set_modifiers(keymap::modifiers(k.meta_state), out);
        let modifiers = self.modifiers;
        if let Some(key) = keymap::key(k.keycode, k.meta_state) {
            if k.pressed {
                if is_cut(modifiers, key) {
                    out.push(Event::Cut);
                    return;
                }
                if is_copy(modifiers, key) {
                    out.push(Event::Copy);
                    return;
                }
                if is_paste(modifiers, key) {
                    if let Some(text) = paste().map(|t| t.replace("\r\n", "\n")).filter(|t| !t.is_empty()) {
                        out.push(Event::Paste(text));
                    }
                    return;
                }
            }
            // egui works out repeats itself.
            out.push(Event::Key { key, physical_key: None, pressed: k.pressed, repeat: false, modifiers });
        }
        if k.pressed
            && keymap::may_type(k.meta_state)
            && !keymap::is_modifier(k.keycode)
            && let Some(text) = k.typed.as_ref().filter(|t| !t.is_empty() && t.chars().all(is_printable))
        {
            out.push(Event::Text(text.clone()));
        }
    }

    /// Forgets held buttons and fingers (the window lost focus or its surface): releases them without a
    /// click, so nothing stays pressed in egui if Android never sends their release.
    pub fn reset(&mut self, out: &mut Vec<Event>) {
        if let Some(pos) = self.pointer {
            let mut held: Vec<PointerButton> = BUTTONS.iter().filter(|(bit, _)| self.buttons & bit != 0).map(|&(_, b)| b).collect();
            if self.touch_pointer.is_some() && !held.contains(&PointerButton::Primary) {
                held.push(PointerButton::Primary);
            }
            self.release_without_click(pos, &held, out);
            self.gone(out);
        }
        self.buttons = 0;
        self.swipe = None;
        self.tap_drag = None;
        self.touch_pointer = None;
        self.set_modifiers(Modifiers::NONE, out);
    }

    fn set_modifiers(&mut self, modifiers: Modifiers, out: &mut Vec<Event>) {
        if modifiers != self.modifiers {
            self.modifiers = modifiers;
            out.push(Event::ModifiersChanged(modifiers));
        }
    }
}

/// Not a control character and not in a private use area (egui-winit's rule).
fn is_printable(c: char) -> bool {
    let private = ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c) || ('\u{100000}'..='\u{10fffd}').contains(&c);
    !private && !c.is_control()
}

// Word's clipboard keys on a PC keyboard: Ctrl+X/C/V, Shift+Delete, Ctrl+Insert, Shift+Insert, and the
// dedicated Cut/Copy/Paste keys.
fn is_cut(m: Modifiers, key: Key) -> bool {
    key == Key::Cut || (m.command && key == Key::X) || (m.shift && !m.command && key == Key::Delete)
}

fn is_copy(m: Modifiers, key: Key) -> bool {
    key == Key::Copy || (m.command && (key == Key::C || key == Key::Insert))
}

fn is_paste(m: Modifiers, key: Key) -> bool {
    key == Key::Paste || (m.command && key == Key::V) || (m.shift && !m.command && key == Key::Insert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{code, meta};

    const CTRL: Modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
    const B: u32 = code::A + 1;
    const C: u32 = code::A + 2;
    const Q: u32 = code::A + 16;
    const V: u32 = code::A + 21;
    const X: u32 = code::A + 23;

    fn finger(id: i32, x: f32, y: f32) -> Pointer {
        Pointer { id, x, y, pressure: 0.5 }
    }

    #[derive(Clone, Copy)]
    enum Device {
        Touch,
        Mouse,
        Touchpad,
    }

    fn motion<'a>(device: Device, action: Action, index: usize, pointers: &'a [Pointer], buttons: u32) -> Motion<'a> {
        let (mouse_source, tool, device_id) = match device {
            Device::Touch => (false, Tool::Finger, 3),
            Device::Mouse => (true, Tool::Mouse, 5),
            Device::Touchpad => (true, Tool::Finger, 7),
        };
        Motion {
            mouse_source,
            tool,
            device_id,
            action,
            index,
            canceled: false,
            no_focus_change: false,
            pointers,
            buttons,
            scroll: Vec2::ZERO,
            meta_state: 0,
        }
    }

    fn pressed(events: &[Event]) -> Vec<(PointerButton, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::PointerButton { button, pressed, .. } => Some((*button, *pressed)),
                _ => None,
            })
            .collect()
    }

    /// Whether egui counts a click when it gets `events` (after a frame with `before`).
    fn egui_clicks(before: Vec<Event>, events: Vec<Event>) -> bool {
        let ctx = egui::Context::default();
        let mut clicked = false;
        for events in [before, events] {
            let raw = egui::RawInput { events, ..Default::default() };
            ctx.run_ui(raw, |_| {}).textures_delta.clear();
            clicked |= ctx.input(|i| i.pointer.any_click());
        }
        clicked
    }

    #[test]
    fn first_finger_drives_the_pointer_like_egui_winit() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let one = [finger(7, 20.0, 40.0)];
        t.motion(&motion(Device::Touch, Action::Down, 0, &one, 0), 2.0, &mut out);
        assert!(matches!(out[0], Event::Touch { phase: TouchPhase::Start, pos, .. } if pos == pos2(10.0, 20.0)));
        assert_eq!(out[1], Event::PointerMoved(pos2(10.0, 20.0)));
        assert_eq!(pressed(&out), [(PointerButton::Primary, true)]);

        // A second finger is only a touch: pinch zoom, no second press.
        out.clear();
        let two = [finger(7, 20.0, 40.0), finger(9, 60.0, 80.0)];
        t.motion(&motion(Device::Touch, Action::PointerDown, 1, &two, 0), 2.0, &mut out);
        assert_eq!(out.len(), 1);
        out.clear();
        t.motion(&motion(Device::Touch, Action::Move, 0, &two, 0), 2.0, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Event::PointerMoved(_))).count(), 1);
        out.clear();
        t.motion(&motion(Device::Touch, Action::PointerUp, 1, &two, 0), 2.0, &mut out);
        assert!(pressed(&out).is_empty());

        out.clear();
        t.motion(&motion(Device::Touch, Action::Up, 0, &one, 0), 2.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Primary, false)]);
        assert_eq!(out.last(), Some(&Event::PointerGone));
    }

    #[test]
    fn a_tap_is_a_click_and_a_cancelled_touch_is_not() {
        let p = [finger(1, 100.0, 100.0)];
        let mut t = Translator::default();
        let (mut down, mut up) = (Vec::new(), Vec::new());
        t.motion(&motion(Device::Touch, Action::Down, 0, &p, 0), 1.0, &mut down);
        t.motion(&motion(Device::Touch, Action::Up, 0, &p, 0), 1.0, &mut up);
        assert!(egui_clicks(down, up), "a tap clicks");

        // A palm Android cancels, as the whole gesture or as one lifted pointer.
        let mut t = Translator::default();
        let (mut down, mut cancel) = (Vec::new(), Vec::new());
        t.motion(&motion(Device::Touch, Action::Down, 0, &p, 0), 1.0, &mut down);
        t.motion(&motion(Device::Touch, Action::Cancel, 0, &p, 0), 1.0, &mut cancel);
        assert_eq!(pressed(&cancel), [(PointerButton::Primary, false)], "the button doesn't stay down");
        assert!(
            matches!(cancel.iter().rev().find(|e| matches!(e, Event::PointerMoved(_))), Some(Event::PointerMoved(p)) if *p == pos2(100.0, 100.0))
        );
        assert!(!egui_clicks(down, cancel), "a cancelled touch doesn't click");

        let mut t = Translator::default();
        let (mut down, mut lifted) = (Vec::new(), Vec::new());
        t.motion(&motion(Device::Touch, Action::Down, 0, &p, 0), 1.0, &mut down);
        let mut up = motion(Device::Touch, Action::Up, 0, &p, 0);
        up.canceled = true;
        t.motion(&up, 1.0, &mut lifted);
        assert!(!egui_clicks(down, lifted));
    }

    #[test]
    fn mouse_buttons_follow_the_button_state_in_any_order() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let p = [finger(0, 10.0, 10.0)];
        // Android: DOWN (primary) then BUTTON_PRESS, … BUTTON_RELEASE then UP.
        t.motion(&motion(Device::Mouse, Action::Down, 0, &p, button::PRIMARY), 1.0, &mut out);
        t.motion(&motion(Device::Mouse, Action::ButtonPress, 0, &p, button::PRIMARY), 1.0, &mut out);
        t.motion(&motion(Device::Mouse, Action::ButtonRelease, 0, &p, 0), 1.0, &mut out);
        t.motion(&motion(Device::Mouse, Action::Up, 0, &p, 0), 1.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Primary, true), (PointerButton::Primary, false)]);

        // Right click: the context menu button.
        out.clear();
        t.motion(&motion(Device::Mouse, Action::ButtonPress, 0, &p, button::SECONDARY), 1.0, &mut out);
        t.motion(&motion(Device::Mouse, Action::ButtonRelease, 0, &p, 0), 1.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Secondary, true), (PointerButton::Secondary, false)]);
        assert!(!out.contains(&Event::PointerGone), "a mouse stays over the window");
    }

    #[test]
    fn a_touchpad_click_is_a_mouse_click() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let p = [finger(0, 10.0, 10.0)];
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &p, button::PRIMARY), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &p, button::PRIMARY), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Up, 0, &p, 0), 1.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Primary, true), (PointerButton::Primary, false)]);
        assert!(!out.iter().any(|e| matches!(e, Event::MouseWheel { .. } | Event::Touch { .. })));
    }

    fn wheel(events: &[Event]) -> Vec<(TouchPhase, Vec2)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, phase, .. } => Some((*phase, *delta)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_two_finger_swipe_scrolls() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &[finger(0, 100.0, 100.0)], 0), 2.0, &mut out);
        assert_eq!(out[0], Event::PointerMoved(pos2(50.0, 50.0)), "the wheel goes to what's under the pointer");
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &[finger(0, 100.0, 140.0)], 0), 2.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &[finger(0, 90.0, 140.0)], 0), 2.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Up, 0, &[finger(0, 90.0, 140.0)], 0), 2.0, &mut out);
        assert_eq!(
            wheel(&out),
            [
                (TouchPhase::Start, Vec2::ZERO),
                (TouchPhase::Move, vec2(0.0, 20.0)),
                (TouchPhase::Move, vec2(-5.0, 0.0)),
                (TouchPhase::End, Vec2::ZERO)
            ]
        );
        assert!(pressed(&out).is_empty(), "a swipe never clicks or selects");
        // The next hover is the pointer again.
        out.clear();
        t.motion(&motion(Device::Touchpad, Action::HoverMove, 0, &[finger(0, 4.0, 6.0)], 0), 2.0, &mut out);
        assert_eq!(out, [Event::PointerMoved(pos2(2.0, 3.0))]);
    }

    #[test]
    fn a_touchpad_tap_clicks_and_a_pinch_zooms() {
        // Tap to click where the fingers landed (Android 10–13 touchpads send no button for a tap).
        let mut t = Translator::default();
        let (mut down, mut up) = (Vec::new(), Vec::new());
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &[finger(0, 40.0, 40.0)], 0), 2.0, &mut down);
        t.motion(&motion(Device::Touchpad, Action::Up, 0, &[finger(0, 41.0, 40.0)], 0), 2.0, &mut up);
        assert_eq!(pressed(&up), [(PointerButton::Primary, true), (PointerButton::Primary, false)]);
        assert!(egui_clicks(down, up));

        // Fingers spreading from 100 to 150 px apart zoom by 1.5, and don't click.
        let mut t = Translator::default();
        let mut out = Vec::new();
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &[finger(0, 100.0, 100.0)], 0), 1.0, &mut out);
        let two = [finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)];
        t.motion(&motion(Device::Touchpad, Action::PointerDown, 1, &two, 0), 1.0, &mut out);
        let wide = [finger(0, 75.0, 100.0), finger(1, 225.0, 100.0)];
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &wide, 0), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::PointerUp, 1, &wide, 0), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Up, 0, &[finger(0, 75.0, 100.0)], 0), 1.0, &mut out);
        let zooms: Vec<f32> = out.iter().filter_map(|e| if let Event::Zoom(z) = e { Some(*z) } else { None }).collect();
        assert_eq!(zooms, [1.5]);
        assert!(wheel(&out).iter().all(|(_, d)| *d == Vec2::ZERO), "a finger landing or lifting doesn't scroll");
        assert!(pressed(&out).is_empty());
    }

    #[test]
    fn an_android_12_touchpad_tap_drag_selects_and_its_swipe_scrolls() {
        let mut t = Translator::default();
        t.set_legacy_touchpad(true);
        let mut out = Vec::new();
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &[finger(0, 10.0, 10.0)], 0), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &[finger(0, 60.0, 10.0)], 0), 1.0, &mut out);
        t.motion(&motion(Device::Touchpad, Action::Up, 0, &[finger(0, 60.0, 10.0)], 0), 1.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Primary, true), (PointerButton::Primary, false)]);
        assert!(out.contains(&Event::PointerMoved(pos2(60.0, 10.0))), "the press drags");
        assert!(wheel(&out).is_empty());

        // The same shape with NO_FOCUS_CHANGE is a two-finger swipe.
        out.clear();
        for (action, x) in [(Action::Down, 10.0), (Action::Move, 60.0), (Action::Up, 60.0)] {
            let p = [finger(0, x, 10.0)];
            let mut m = motion(Device::Touchpad, action, 0, &p, 0);
            m.no_focus_change = true;
            t.motion(&m, 1.0, &mut out);
        }
        assert!(pressed(&out).is_empty());
        assert_eq!(wheel(&out).len(), 3);
    }

    #[test]
    fn a_touch_during_a_swipe_is_its_own_gesture() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        t.motion(&motion(Device::Touchpad, Action::Down, 0, &[finger(0, 10.0, 10.0)], 0), 1.0, &mut out);
        out.clear();
        t.motion(&motion(Device::Touch, Action::Down, 0, &[finger(0, 500.0, 500.0)], 0), 1.0, &mut out);
        assert_eq!(pressed(&out), [(PointerButton::Primary, true)], "the screen still presses");
        out.clear();
        t.motion(&motion(Device::Touchpad, Action::Move, 0, &[finger(0, 10.0, 30.0)], 0), 1.0, &mut out);
        assert_eq!(wheel(&out), [(TouchPhase::Move, vec2(0.0, 20.0))], "the swipe scrolls by its own movement");
    }

    #[test]
    fn hover_moves_and_leaves_and_the_wheel_scrolls_where_the_pointer_is() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let p = [finger(0, 30.0, 60.0)];
        t.motion(&motion(Device::Mouse, Action::HoverMove, 0, &p, 0), 3.0, &mut out);
        assert_eq!(out, [Event::PointerMoved(pos2(10.0, 20.0))]);
        out.clear();
        t.motion(&motion(Device::Mouse, Action::HoverExit, 0, &p, 0), 3.0, &mut out);
        assert_eq!(out, [Event::PointerGone], "no hover or tooltip sticks");
        out.clear();
        let mut wheel = motion(Device::Mouse, Action::Scroll, 0, &p, 0);
        wheel.scroll = vec2(0.0, 1.0);
        t.motion(&wheel, 3.0, &mut out);
        assert_eq!(out[0], Event::PointerMoved(pos2(10.0, 20.0)));
        assert!(matches!(out[1], Event::MouseWheel { delta, unit: egui::MouseWheelUnit::Line, .. } if delta == vec2(0.0, 1.0)));
        out.clear();
        wheel.scroll = vec2(1.0, 0.0);
        t.motion(&wheel, 3.0, &mut out);
        assert!(matches!(out[1], Event::MouseWheel { delta, .. } if delta == vec2(-1.0, 0.0)), "right scrolls the content left");
    }

    #[test]
    fn losing_focus_releases_without_a_click() {
        let mut t = Translator::default();
        let (mut down, mut reset) = (Vec::new(), Vec::new());
        let p = [finger(0, 50.0, 50.0)];
        t.motion(&motion(Device::Mouse, Action::Down, 0, &p, button::PRIMARY), 1.0, &mut down);
        t.reset(&mut reset);
        assert_eq!(pressed(&reset), [(PointerButton::Primary, false)]);
        assert!(!egui_clicks(down, reset));
        // The release Android sends later presses nothing again.
        let mut out = Vec::new();
        t.motion(&motion(Device::Mouse, Action::Up, 0, &p, 0), 1.0, &mut out);
        assert!(pressed(&out).is_empty());
    }

    #[test]
    fn bad_input_never_panics() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let p = [Pointer { id: i32::MIN, x: f32::NAN, y: f32::INFINITY, pressure: f32::NAN }, finger(i32::MAX, f32::MAX, -f32::MAX)];
        let actions = [
            Action::Down,
            Action::Move,
            Action::Up,
            Action::Cancel,
            Action::PointerDown,
            Action::PointerUp,
            Action::HoverEnter,
            Action::HoverMove,
            Action::HoverExit,
            Action::Scroll,
            Action::ButtonPress,
            Action::ButtonRelease,
            Action::Other,
        ];
        for action in actions {
            for device in [Device::Touch, Device::Mouse, Device::Touchpad] {
                t.motion(&motion(device, action, usize::MAX, &p, u32::MAX), 0.0, &mut out);
                t.motion(&motion(device, action, 0, &p, 0), f32::NAN, &mut out);
                t.motion(&motion(device, action, 0, &[], u32::MAX), 1.0, &mut out);
            }
        }
        t.reset(&mut out);
        t.sync_modifiers(u32::MAX, &mut out);
    }

    fn key(t: &mut Translator, keycode: u32, meta_state: u32, typed: Option<&str>, clip: &str) -> Vec<Event> {
        let mut out = Vec::new();
        let clip = clip.to_owned();
        t.key(&KeyPress { keycode, meta_state, pressed: true, typed: typed.map(str::to_owned) }, move || Some(clip), &mut out);
        out
    }

    #[test]
    fn keys_type_text_unless_a_shortcut() {
        let mut t = Translator::default();
        let out = key(&mut t, code::A, 0, Some("a"), "");
        assert!(matches!(out[0], Event::Key { key: Key::A, pressed: true, .. }));
        assert_eq!(out[1], Event::Text("a".into()));

        let out = key(&mut t, B, meta::CTRL_ON, Some("b"), "");
        assert_eq!(out[0], Event::ModifiersChanged(CTRL));
        assert!(matches!(out[1], Event::Key { key: Key::B, modifiers, .. } if modifiers.command));
        assert_eq!(out.len(), 2, "Ctrl+B doesn't type a b");

        // AltGr (Right Alt) types; a key without an egui name still types; a dead key's accent comes first.
        let out = key(&mut t, Q, meta::ALT_ON, Some("@"), "");
        assert!(out.contains(&Event::Text("@".into())));
        let out = key(&mut t, 77, 0, Some("@"), "");
        assert_eq!(out.last(), Some(&Event::Text("@".into())));
        let out = key(&mut t, X, 0, Some("^x"), "");
        assert_eq!(out.last(), Some(&Event::Text("^x".into())));
        // Enter and Tab are keys, not text.
        let out = key(&mut t, code::ENTER, 0, Some("\n"), "");
        assert!(!out.iter().any(|e| matches!(e, Event::Text(_))));
    }

    #[test]
    fn modifiers_follow_the_newest_event() {
        let mut t = Translator::default();
        let mut out = Vec::new();
        let p = [finger(0, 1.0, 1.0)];
        let mut wheel = motion(Device::Mouse, Action::Scroll, 0, &p, 0);
        wheel.meta_state = meta::CTRL_ON;
        t.motion(&wheel, 1.0, &mut out);
        assert_eq!(out.first(), Some(&Event::ModifiersChanged(CTRL)));
        out.clear();
        t.sync_modifiers(0, &mut out);
        assert_eq!(out, [Event::ModifiersChanged(Modifiers::NONE)]);
    }

    #[test]
    fn clipboard_shortcuts() {
        let mut t = Translator::default();
        assert_eq!(key(&mut t, C, meta::CTRL_ON, None, ""), [Event::ModifiersChanged(CTRL), Event::Copy]);
        assert_eq!(key(&mut t, X, meta::CTRL_ON, None, ""), [Event::Cut]);
        assert_eq!(key(&mut t, V, meta::CTRL_ON, None, "a\r\nb"), [Event::Paste("a\nb".into())]);
        assert_eq!(key(&mut t, V, meta::CTRL_ON, None, ""), [], "an empty clipboard pastes nothing");
        assert_eq!(key(&mut t, code::INSERT, meta::CTRL_ON, None, ""), [Event::Copy]);
        let out = key(&mut t, code::INSERT, meta::SHIFT_ON, None, "x");
        assert_eq!(out.last(), Some(&Event::Paste("x".into())));
        let out = key(&mut t, code::FORWARD_DEL, meta::SHIFT_ON, None, "");
        assert_eq!(out.last(), Some(&Event::Cut));
        assert_eq!(key(&mut t, code::PASTE, 0, None, "y").last(), Some(&Event::Paste("y".into())));
        // The paste shortcut's release still reaches the app (keys.rs waits for it).
        let mut out = Vec::new();
        t.key(&KeyPress { keycode: V, meta_state: meta::CTRL_ON, pressed: false, typed: None }, || None, &mut out);
        assert!(matches!(out.last(), Some(Event::Key { key: Key::V, pressed: false, .. })));
    }

    #[test]
    fn modifier_keys_are_keys_for_keytips() {
        let mut t = Translator::default();
        let out = key(&mut t, code::ALT_LEFT, meta::ALT_ON, None, "");
        assert!(out.iter().any(|e| matches!(e, Event::Key { key: Key::AltLeft, .. })));
    }
}
