//! Android key codes and meta state → egui keys and modifiers.
//!
//! Pure data, so it builds and is tested on the host. The numbers are the public, stable
//! `android.view.KeyEvent` constants (`AKEYCODE_*`/`AMETA_*` in the NDK); `android_activity::input::Keycode`
//! converts into them with `u32::from`.

use egui::{Key, Modifiers};

/// `AMETA_*` bits of a key or motion event's meta state.
pub mod meta {
    pub const SHIFT_ON: u32 = 0x1;
    pub const ALT_ON: u32 = 0x2;
    pub const CTRL_ON: u32 = 0x1000;
    pub const META_ON: u32 = 0x1_0000;
    pub const NUM_LOCK_ON: u32 = 0x20_0000;
}

/// The `AKEYCODE_*` values this host maps.
pub mod code {
    pub const BACK: u32 = 4;
    pub const NUM_0: u32 = 7;
    pub const NUM_9: u32 = 16;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const A: u32 = 29;
    pub const Z: u32 = 54;
    pub const COMMA: u32 = 55;
    pub const PERIOD: u32 = 56;
    pub const ALT_LEFT: u32 = 57;
    pub const ALT_RIGHT: u32 = 58;
    pub const SHIFT_LEFT: u32 = 59;
    pub const SHIFT_RIGHT: u32 = 60;
    pub const TAB: u32 = 61;
    pub const SPACE: u32 = 62;
    pub const ENTER: u32 = 66;
    /// Backspace.
    pub const DEL: u32 = 67;
    pub const GRAVE: u32 = 68;
    pub const MINUS: u32 = 69;
    pub const EQUALS: u32 = 70;
    pub const LEFT_BRACKET: u32 = 71;
    pub const RIGHT_BRACKET: u32 = 72;
    pub const BACKSLASH: u32 = 73;
    pub const SEMICOLON: u32 = 74;
    pub const APOSTROPHE: u32 = 75;
    pub const SLASH: u32 = 76;
    pub const PLUS: u32 = 81;
    pub const PAGE_UP: u32 = 92;
    pub const PAGE_DOWN: u32 = 93;
    pub const ESCAPE: u32 = 111;
    /// Delete (forward).
    pub const FORWARD_DEL: u32 = 112;
    pub const CTRL_LEFT: u32 = 113;
    pub const CTRL_RIGHT: u32 = 114;
    pub const META_LEFT: u32 = 117;
    pub const META_RIGHT: u32 = 118;
    pub const MOVE_HOME: u32 = 122;
    pub const MOVE_END: u32 = 123;
    pub const INSERT: u32 = 124;
    pub const F1: u32 = 131;
    pub const F12: u32 = 142;
    pub const NUMPAD_0: u32 = 144;
    pub const NUMPAD_1: u32 = 145;
    pub const NUMPAD_2: u32 = 146;
    pub const NUMPAD_3: u32 = 147;
    pub const NUMPAD_4: u32 = 148;
    pub const NUMPAD_6: u32 = 150;
    pub const NUMPAD_7: u32 = 151;
    pub const NUMPAD_8: u32 = 152;
    pub const NUMPAD_9: u32 = 153;
    pub const NUMPAD_DIVIDE: u32 = 154;
    pub const NUMPAD_SUBTRACT: u32 = 156;
    pub const NUMPAD_ADD: u32 = 157;
    pub const NUMPAD_DOT: u32 = 158;
    pub const NUMPAD_COMMA: u32 = 159;
    pub const NUMPAD_ENTER: u32 = 160;
    pub const NUMPAD_EQUALS: u32 = 161;
    pub const CUT: u32 = 277;
    pub const COPY: u32 = 278;
    pub const PASTE: u32 = 279;
}

/// egui's modifiers for a meta state. Android has no Command key: Ctrl is the command modifier, as on
/// Windows and Linux. The Meta (Windows/Search) key isn't a text-editing modifier and is left out.
pub fn modifiers(meta_state: u32) -> Modifiers {
    let ctrl = meta_state & meta::CTRL_ON != 0;
    Modifiers { alt: meta_state & meta::ALT_ON != 0, ctrl, shift: meta_state & meta::SHIFT_ON != 0, mac_cmd: false, command: ctrl }
}

/// Whether a key press with this meta state may type text: not while Ctrl or Meta is held (those are
/// shortcuts). Alt may type: AltGr is Right Alt on Android keyboards.
pub fn may_type(meta_state: u32) -> bool {
    meta_state & (meta::CTRL_ON | meta::META_ON) == 0
}

/// egui's key for an Android key code, or `None` for keys egui has no name for (they may still type
/// text through the key character map). The numeric keypad follows Num Lock like a PC keypad.
pub fn key(keycode: u32, meta_state: u32) -> Option<Key> {
    let num_lock = meta_state & meta::NUM_LOCK_ON != 0;
    Some(match keycode {
        code::A..=code::Z => letter(keycode - code::A)?,
        code::NUM_0..=code::NUM_9 => digit(keycode - code::NUM_0)?,
        code::NUMPAD_0..=code::NUMPAD_9 if num_lock => digit(keycode - code::NUMPAD_0)?,
        code::NUMPAD_0 => Key::Insert,
        code::NUMPAD_1 => Key::End,
        code::NUMPAD_2 => Key::ArrowDown,
        code::NUMPAD_3 => Key::PageDown,
        code::NUMPAD_4 => Key::ArrowLeft,
        code::NUMPAD_6 => Key::ArrowRight,
        code::NUMPAD_7 => Key::Home,
        code::NUMPAD_8 => Key::ArrowUp,
        code::NUMPAD_9 => Key::PageUp,
        code::NUMPAD_DOT if num_lock => Key::Period,
        code::NUMPAD_DOT => Key::Delete,
        code::F1..=code::F12 => function(keycode - code::F1)?,
        code::DPAD_UP => Key::ArrowUp,
        code::DPAD_DOWN => Key::ArrowDown,
        code::DPAD_LEFT => Key::ArrowLeft,
        code::DPAD_RIGHT => Key::ArrowRight,
        code::ENTER | code::NUMPAD_ENTER => Key::Enter,
        code::TAB => Key::Tab,
        code::SPACE => Key::Space,
        code::DEL => Key::Backspace,
        code::FORWARD_DEL => Key::Delete,
        // GameActivity takes Back from the system (and the activity hands over the predictive back
        // gesture): it closes what's open, like Esc. Nothing here quits the app on Back.
        code::ESCAPE | code::BACK => Key::Escape,
        code::INSERT => Key::Insert,
        code::MOVE_HOME => Key::Home,
        code::MOVE_END => Key::End,
        code::PAGE_UP => Key::PageUp,
        code::PAGE_DOWN => Key::PageDown,
        code::COMMA | code::NUMPAD_COMMA => Key::Comma,
        code::PERIOD => Key::Period,
        code::GRAVE => Key::Backtick,
        code::MINUS | code::NUMPAD_SUBTRACT => Key::Minus,
        code::EQUALS | code::NUMPAD_EQUALS => Key::Equals,
        code::PLUS | code::NUMPAD_ADD => Key::Plus,
        code::LEFT_BRACKET => Key::OpenBracket,
        code::RIGHT_BRACKET => Key::CloseBracket,
        code::BACKSLASH => Key::Backslash,
        code::SEMICOLON => Key::Semicolon,
        code::APOSTROPHE => Key::Quote,
        code::SLASH | code::NUMPAD_DIVIDE => Key::Slash,
        code::SHIFT_LEFT => Key::ShiftLeft,
        code::SHIFT_RIGHT => Key::ShiftRight,
        code::CTRL_LEFT => Key::ControlLeft,
        code::CTRL_RIGHT => Key::ControlRight,
        code::ALT_LEFT => Key::AltLeft,
        code::ALT_RIGHT => Key::AltRight,
        code::META_LEFT => Key::SuperLeft,
        code::META_RIGHT => Key::SuperRight,
        code::CUT => Key::Cut,
        code::COPY => Key::Copy,
        code::PASTE => Key::Paste,
        _ => return None,
    })
}

/// Whether the key is a modifier on its own (Shift, Ctrl, Alt, Meta): it never types text.
pub fn is_modifier(keycode: u32) -> bool {
    matches!(
        keycode,
        code::SHIFT_LEFT
            | code::SHIFT_RIGHT
            | code::CTRL_LEFT
            | code::CTRL_RIGHT
            | code::ALT_LEFT
            | code::ALT_RIGHT
            | code::META_LEFT
            | code::META_RIGHT
    )
}

/// What a key types, as Android's `KeyCharacterMap.get` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyChar {
    None,
    Unicode(char),
    /// A dead key: this accent combines with the next character.
    Accent(char),
}

impl KeyChar {
    /// From `KeyCharacterMap.get`'s result: 0 is nothing, and `COMBINING_ACCENT` (the top bit) marks a dead
    /// key's accent.
    pub fn from_java(value: i32) -> Self {
        const COMBINING_ACCENT: u32 = 0x8000_0000;
        let bits = u32::from_ne_bytes(value.to_ne_bytes());
        if bits & COMBINING_ACCENT != 0 {
            char::from_u32(bits & !COMBINING_ACCENT).map_or(Self::None, Self::Accent)
        } else {
            char::from_u32(bits).filter(|&c| c != '\0').map_or(Self::None, Self::Unicode)
        }
    }
}

/// A pending dead key, combined with the next character the way text fields on Android and Windows do.
#[derive(Debug, Default)]
pub struct DeadKeys {
    accent: Option<char>,
}

impl DeadKeys {
    /// The text a key press types. `combine(accent, c)` is the keyboard's composition (Android's
    /// `KeyCharacterMap.getDeadChar`), `None` when the two don't combine.
    pub fn press(&mut self, keycode: u32, ch: KeyChar, combine: impl FnOnce(char, char) -> Option<char>) -> Option<String> {
        match ch {
            KeyChar::Accent(accent) => match self.accent.replace(accent) {
                // The same dead key twice types its accent.
                Some(pending) if pending == accent => {
                    self.accent = None;
                    Some(accent.to_string())
                }
                // Another dead key: the pending accent is typed, the new one waits.
                Some(pending) => Some(pending.to_string()),
                None => None,
            },
            KeyChar::Unicode(c) => Some(match self.accent.take() {
                Some(accent) => combine(accent, c).map_or_else(|| format!("{accent}{c}"), String::from),
                None => c.to_string(),
            }),
            // A key that types nothing (an arrow, Backspace) drops the accent; a modifier doesn't.
            KeyChar::None => {
                if !is_modifier(keycode) {
                    self.accent = None;
                }
                None
            }
        }
    }

    /// Forgets a pending accent (the window lost focus).
    pub fn clear(&mut self) {
        self.accent = None;
    }
}

fn letter(i: u32) -> Option<Key> {
    const LETTERS: [Key; 26] = [
        Key::A,
        Key::B,
        Key::C,
        Key::D,
        Key::E,
        Key::F,
        Key::G,
        Key::H,
        Key::I,
        Key::J,
        Key::K,
        Key::L,
        Key::M,
        Key::N,
        Key::O,
        Key::P,
        Key::Q,
        Key::R,
        Key::S,
        Key::T,
        Key::U,
        Key::V,
        Key::W,
        Key::X,
        Key::Y,
        Key::Z,
    ];
    LETTERS.get(usize::try_from(i).ok()?).copied()
}

fn digit(i: u32) -> Option<Key> {
    const DIGITS: [Key; 10] = [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
    DIGITS.get(usize::try_from(i).ok()?).copied()
}

fn function(i: u32) -> Option<Key> {
    const KEYS: [Key; 12] = [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12];
    KEYS.get(usize::try_from(i).ok()?).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_function_keys() {
        assert_eq!(key(code::A, 0), Some(Key::A));
        assert_eq!(key(code::Z, 0), Some(Key::Z));
        assert_eq!(key(code::A + 1, 0), Some(Key::B));
        assert_eq!(key(code::NUM_0, 0), Some(Key::Num0));
        assert_eq!(key(code::NUM_9, 0), Some(Key::Num9));
        assert_eq!(key(code::F1, 0), Some(Key::F1));
        assert_eq!(key(code::F1 + 3, 0), Some(Key::F4));
        assert_eq!(key(code::F12, 0), Some(Key::F12));
    }

    #[test]
    fn editing_and_navigation_keys() {
        assert_eq!(key(code::DEL, 0), Some(Key::Backspace));
        assert_eq!(key(code::FORWARD_DEL, 0), Some(Key::Delete));
        assert_eq!(key(code::MOVE_HOME, 0), Some(Key::Home));
        assert_eq!(key(code::MOVE_END, 0), Some(Key::End));
        assert_eq!(key(code::PAGE_UP, 0), Some(Key::PageUp));
        assert_eq!(key(code::DPAD_LEFT, 0), Some(Key::ArrowLeft));
        assert_eq!(key(code::NUMPAD_ENTER, 0), Some(Key::Enter));
        assert_eq!(key(code::BACK, 0), Some(Key::Escape), "Back closes what's open");
        assert_eq!(key(u32::MAX, 0), None);
    }

    #[test]
    fn numpad_follows_num_lock() {
        assert_eq!(key(code::NUMPAD_7, meta::NUM_LOCK_ON), Some(Key::Num7));
        assert_eq!(key(code::NUMPAD_7, 0), Some(Key::Home));
        assert_eq!(key(code::NUMPAD_DOT, meta::NUM_LOCK_ON), Some(Key::Period));
        assert_eq!(key(code::NUMPAD_DOT, 0), Some(Key::Delete));
        assert_eq!(key(code::NUMPAD_ADD, 0), Some(Key::Plus));
    }

    #[test]
    fn ctrl_is_the_command_modifier() {
        let m = modifiers(meta::CTRL_ON | meta::SHIFT_ON);
        assert!(m.ctrl && m.command && m.shift && !m.alt && !m.mac_cmd);
        assert_eq!(modifiers(0), Modifiers::NONE);
        assert!(modifiers(meta::ALT_ON).alt);
    }

    #[test]
    fn shortcuts_do_not_type() {
        assert!(may_type(0));
        assert!(may_type(meta::SHIFT_ON | meta::ALT_ON));
        assert!(!may_type(meta::CTRL_ON));
        assert!(!may_type(meta::META_ON));
        assert!(is_modifier(code::CTRL_LEFT) && !is_modifier(code::A));
    }

    #[test]
    fn key_chars_from_java() {
        assert_eq!(KeyChar::from_java(0), KeyChar::None);
        assert_eq!(KeyChar::from_java('a' as i32), KeyChar::Unicode('a'));
        assert_eq!(KeyChar::from_java(i32::from_ne_bytes((0x8000_0000u32 | 0xB4).to_ne_bytes())), KeyChar::Accent('´'));
        assert_eq!(KeyChar::from_java(0xD800), KeyChar::None, "a lone surrogate isn't a char");
        assert_eq!(KeyChar::from_java(-1), KeyChar::None);
    }

    #[test]
    fn dead_keys_combine_like_text_fields() {
        let acute = |a: char, c: char| (a == '´' && c == 'e').then_some('é');
        let mut d = DeadKeys::default();
        assert_eq!(d.press(0, KeyChar::Accent('´'), acute), None);
        assert_eq!(d.press(0, KeyChar::Unicode('e'), acute), Some("é".into()));
        // No composition: the accent, then the character.
        d.press(0, KeyChar::Accent('^'), acute);
        assert_eq!(d.press(0, KeyChar::Unicode('x'), |_, _| None), Some("^x".into()));
        // The same dead key twice types the accent; another dead key types the pending one.
        d.press(0, KeyChar::Accent('^'), acute);
        assert_eq!(d.press(0, KeyChar::Accent('^'), acute), Some("^".into()));
        d.press(0, KeyChar::Accent('^'), acute);
        assert_eq!(d.press(0, KeyChar::Accent('´'), acute), Some("^".into()));
        assert_eq!(d.press(0, KeyChar::Unicode('e'), acute), Some("é".into()));
        // Shift between the accent and the letter keeps it; an arrow or losing focus drops it.
        d.press(0, KeyChar::Accent('´'), acute);
        assert_eq!(d.press(code::SHIFT_LEFT, KeyChar::None, acute), None);
        assert_eq!(d.press(0, KeyChar::Unicode('e'), acute), Some("é".into()));
        d.press(0, KeyChar::Accent('´'), acute);
        d.press(code::DPAD_LEFT, KeyChar::None, acute);
        assert_eq!(d.press(0, KeyChar::Unicode('e'), acute), Some("e".into()));
        d.press(0, KeyChar::Accent('´'), acute);
        d.clear();
        assert_eq!(d.press(0, KeyChar::Unicode('e'), acute), Some("e".into()));
    }
}
