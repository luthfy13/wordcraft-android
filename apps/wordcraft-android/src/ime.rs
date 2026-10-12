//! The on-screen keyboard (an IME, input method): composition, suggestions, AutoCorrect and swiping work
//! on the document's own text.
//!
//! Android's IMEs edit a text buffer they can read (GameTextInput's, in the activity). The host keeps that
//! buffer holding a **window of the document around the caret** (paragraphs joined by `"\n"`), and keeps a
//! *shadow* of it here. Each state the IME reports is compared with the shadow; the difference becomes
//! ordinary key and text events at the caret (Backspace, Delete, typed text, the composition shown as
//! egui's preedit), so the document changes through the same commands as with a keyboard: Undo,
//! AutoCorrect, AutoFormat and Track Changes all apply. When the document no longer matches what the
//! IME holds (the app changed it, the caret moved), the host *pushes* a fresh window.
//!
//! Indices from Android are UTF-16 units and may be anything: they're clamped. Each push gets a
//! generation, which the activity tags the IME's states with, so a state based on an older push is
//! recognised.
//!
//! Targets: the document, an equation (a buffer of spaces, whose deletions are Backspaces), and dialog
//! fields (their text as egui reports it).

use std::collections::VecDeque;

use egui::{Event, ImeEvent, Key, Modifiers};
use wordcraft_doc::{Document, cluster};
use wordcraft_engine::Selection;

/// Text shown to the IME before and after the caret, in UTF-16 units (LatinIME caches 1024 before).
pub const BEFORE: usize = 800;
pub const AFTER: usize = 400;
/// A buffer this long is replaced by a fresh window at the next quiet moment; at `HARD`, at once.
const SOFT: usize = 4096;
const HARD: usize = 16384;
/// Pushes whose states are still recognised.
const HISTORY: usize = 4;
/// An equation's buffer: deletions in it are Backspaces, and it's refilled when it runs low.
const SENTINEL: usize = 8;

/// An IME buffer, indexed in `char`s.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Buf {
    pub text: Vec<char>,
    pub anchor: usize,
    pub focus: usize,
    pub compose: Option<(usize, usize)>,
}

/// A UTF-16 index as Android sends it (anything) to a `char` index: clamped, and rounded down inside a
/// surrogate pair.
pub fn utf16_to_char(text: &[char], u: i64) -> usize {
    let Ok(mut left) = usize::try_from(u) else { return 0 };
    for (i, c) in text.iter().enumerate() {
        let w = c.len_utf16();
        if left < w {
            return i;
        }
        left -= w;
    }
    text.len()
}

pub fn char_to_utf16(text: &[char], c: usize) -> usize {
    text.iter().take(c).map(|c| c.len_utf16()).sum()
}

fn utf16_len(text: &[char]) -> usize {
    text.iter().map(|c| c.len_utf16()).sum()
}

/// A state as Android reports it to a buffer.
pub fn normalize(text: &str, sel: (i64, i64), compose: Option<(i64, i64)>) -> Buf {
    let text: Vec<char> = text.chars().collect();
    let anchor = utf16_to_char(&text, sel.0);
    let focus = utf16_to_char(&text, sel.1);
    let compose = compose.and_then(|(a, b)| {
        let (a, b) = (utf16_to_char(&text, a.min(b)), utf16_to_char(&text, a.max(b)));
        (a < b).then_some((a, b))
    });
    Buf { text, anchor, focus, compose }
}

/// What an IME state means for the document, in `char`s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// End the preedit before editing.
    pub clear_preedit: bool,
    /// The document's selection goes: the inserted text replaces it, or one Backspace removes it.
    pub replace_selection: bool,
    pub backspace: usize,
    pub delete: usize,
    /// Text committed at the caret.
    pub insert: String,
    /// The new preedit, when it changed (`Some("")` ends it).
    pub preedit: Option<String>,
    /// The anchor and focus relative to the caret after the edits, when the IME moved them.
    pub select: Option<(isize, isize)>,
    /// The document text before the caret (for counting Backspaces as the app does).
    before: String,
    /// And after it.
    after: String,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        !self.clear_preedit
            && !self.replace_selection
            && self.backspace == 0
            && self.delete == 0
            && self.insert.is_empty()
            && self.preedit.is_none()
            && self.select.is_none()
    }
}

/// What the IME's buffer holds, and which of its characters are in the document (the others are the
/// composition, shown at the caret but not yet typed).
#[derive(Clone, Debug)]
pub struct Shadow {
    buf: Buf,
    in_doc: Vec<bool>,
}

struct View {
    doc: Vec<char>,
    caret: usize,
    anchor: usize,
    preedit: String,
}

fn common_prefix(a: &[char], b: &[char]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[char], b: &[char]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

impl Shadow {
    /// After a push: all of it in the document, nothing composing.
    pub fn pushed(mut buf: Buf) -> Self {
        buf.compose = None;
        let in_doc = vec![true; buf.text.len()];
        Self { buf, in_doc }
    }

    pub fn preedit_active(&self) -> bool {
        self.in_doc.iter().any(|d| !d)
    }

    fn len_utf16(&self) -> usize {
        utf16_len(&self.buf.text)
    }

    fn view(buf: &Buf, in_doc: &[bool]) -> View {
        let doc: Vec<char> = buf.text.iter().zip(in_doc).filter(|(_, d)| **d).map(|(c, _)| *c).collect();
        let doc_index = |i: usize| in_doc.iter().take(i).filter(|d| **d).count();
        match in_doc.iter().position(|d| !d) {
            Some(s) => {
                let caret = doc_index(s);
                let preedit = buf.text.iter().zip(in_doc).filter(|(_, d)| !**d).map(|(c, _)| *c).collect();
                View { doc, caret, anchor: caret, preedit }
            }
            None => View { doc, caret: doc_index(buf.focus), anchor: doc_index(buf.anchor), preedit: String::new() },
        }
    }

    /// A new state from the IME: what brings the document and the preedit in line with it.
    pub fn apply(&mut self, new: Buf) -> Plan {
        let old = &self.buf;
        let (lo, ln) = (old.text.len(), new.text.len());
        let p = common_prefix(&old.text, &new.text);
        let q = common_suffix(old.text.get(p..).unwrap_or(&[]), new.text.get(p..).unwrap_or(&[]));
        let in_comp = |i: usize| new.compose.is_some_and(|(s, e)| s <= i && i < e);
        // Unchanged characters keep their flag; changed ones are composition inside the composing region
        // and typed outside it.
        let mut flags: Vec<bool> = (0..ln)
            .map(|i| {
                if i < p {
                    self.in_doc.get(i).copied().unwrap_or(true)
                } else if i >= ln.saturating_sub(q) {
                    self.in_doc.get((i + lo).saturating_sub(ln)).copied().unwrap_or(true)
                } else {
                    !in_comp(i)
                }
            })
            .collect();
        for (i, f) in flags.iter_mut().enumerate() {
            if !in_comp(i) {
                *f = true;
            }
        }
        // egui has one preedit, at the caret: two runs make the whole region the composition.
        let runs =
            flags.windows(2).filter(|w| w.first() == Some(&true) && w.get(1) == Some(&false)).count() + usize::from(flags.first() == Some(&false));
        if runs > 1
            && let Some((s, e)) = new.compose
        {
            for f in flags.get_mut(s..e).into_iter().flatten() {
                *f = false;
            }
        }
        let a = Self::view(old, &self.in_doc);
        let b = Self::view(&new, &flags);
        let mut plan = Plan::default();
        let mut caret_after = a.caret;
        if a.doc != b.doc {
            let pd = common_prefix(&a.doc, &b.doc);
            let qd = common_suffix(a.doc.get(pd..).unwrap_or(&[]), b.doc.get(pd..).unwrap_or(&[]));
            let (sel_lo, sel_hi) = (a.anchor.min(a.caret), a.anchor.max(a.caret));
            // Widened to reach the caret and the selection: every edit is relative to the caret.
            let start = pd.min(sel_lo);
            let end_old = a.doc.len().saturating_sub(qd).max(sel_hi);
            let end_new = b.doc.len().saturating_sub(a.doc.len().saturating_sub(end_old));
            plan.insert = b.doc.get(start..end_new.max(start)).unwrap_or(&[]).iter().collect();
            if sel_lo < sel_hi {
                plan.replace_selection = true;
                if !(start == sel_lo && end_old == sel_hi && !plan.insert.is_empty()) {
                    plan.backspace = sel_lo.saturating_sub(start);
                    plan.delete = end_old.saturating_sub(sel_hi);
                }
                plan.before = a.doc.get(..sel_lo).unwrap_or(&[]).iter().collect();
                plan.after = a.doc.get(sel_hi..).unwrap_or(&[]).iter().collect();
            } else {
                plan.backspace = a.caret.saturating_sub(start);
                plan.delete = end_old.saturating_sub(a.caret);
                plan.before = a.doc.get(..a.caret).unwrap_or(&[]).iter().collect();
                plan.after = a.doc.get(a.caret..).unwrap_or(&[]).iter().collect();
            }
            caret_after = end_new;
        }
        if (b.anchor, b.caret) != (caret_after, caret_after) && !(a.doc == b.doc && (b.anchor, b.caret) == (a.anchor, a.caret)) {
            plan.select = Some((signed(b.anchor) - signed(caret_after), signed(b.caret) - signed(caret_after)));
        }
        let edits = a.doc != b.doc || plan.select.is_some();
        if edits && !a.preedit.is_empty() {
            plan.clear_preedit = true;
            if !b.preedit.is_empty() {
                plan.preedit = Some(b.preedit.clone());
            }
        } else if a.preedit != b.preedit {
            plan.preedit = Some(b.preedit.clone());
        }
        self.buf = new;
        self.in_doc = flags;
        plan
    }

    /// Whether a fresh window (text before and after the caret, anchor relative to it) agrees with what
    /// the IME holds. Windows cut at other lengths agree on their overlap, so typing never forces a push.
    pub fn consistent(&self, fresh: &Window) -> bool {
        let v = Self::view(&self.buf, &self.in_doc);
        let (b, a) = v.doc.split_at(v.caret.min(v.doc.len()));
        let before_ok = b.ends_with(&fresh.before) || fresh.before.ends_with(b);
        let after_ok = a.starts_with(&fresh.after) || fresh.after.starts_with(a);
        let anchor_ok = !v.preedit.is_empty() || signed(v.anchor) - signed(v.caret) == fresh.anchor;
        before_ok && after_ok && anchor_ok
    }

    /// The composition ends where it is (a tap or a key comes): it's typed.
    pub fn commit_preedit(&mut self) -> Plan {
        let mut b = self.buf.clone();
        b.compose = None;
        self.apply(b)
    }
}

/// What the IME should hold: the text around the caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub before: Vec<char>,
    pub after: Vec<char>,
    /// The anchor relative to the caret, in `char`s (0: no selection, or one the window can't show).
    pub anchor: isize,
}

/// The end of `text` within `budget` UTF-16 units; whether all of it fit.
fn tail(text: &str, budget: usize) -> (Vec<char>, bool) {
    let mut used = 0;
    let mut out: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        used += c.len_utf16();
        if used > budget {
            out.reverse();
            return (out, false);
        }
        out.push(c);
    }
    out.reverse();
    (out, true)
}

fn head(text: &str, budget: usize) -> (Vec<char>, bool) {
    let mut used = 0;
    let mut out = Vec::new();
    for c in text.chars() {
        used += c.len_utf16();
        if used > budget {
            return (out, false);
        }
        out.push(c);
    }
    (out, true)
}

/// `off` moved back to a character boundary of `text`.
fn floor(text: &str, off: usize) -> usize {
    let mut o = off.min(text.len());
    while o > 0 && !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

impl Window {
    /// The document around the selection's focus: its paragraph, and the neighbouring paragraphs of the
    /// same story and container when the window reaches them (Backspace at a paragraph's start joins it to
    /// the one before).
    pub fn of_document(doc: &Document, sel: &Selection) -> Option<Self> {
        let focus = &sel.focus;
        let para = doc.para(focus.story, &focus.path)?;
        let off = floor(&para.text, focus.off);
        let (mut before, whole_start) = tail(para.text.get(..off)?, BEFORE);
        if whole_start
            && let Some(i) = focus.path.last().checked_sub(1)
            && let Some(prev) = doc.para(focus.story, &focus.path.with_last(i))
        {
            let budget = BEFORE.saturating_sub(utf16_len(&before)).saturating_sub(1);
            let (mut text, _) = tail(&prev.text, budget);
            text.push('\n');
            text.extend(before);
            before = text;
        }
        let (mut after, whole_end) = head(para.text.get(off..)?, AFTER);
        if whole_end && let Some(next) = doc.para(focus.story, &focus.path.with_last(focus.path.last().saturating_add(1))) {
            let budget = AFTER.saturating_sub(utf16_len(&after)).saturating_sub(1);
            after.push('\n');
            after.extend(head(&next.text, budget).0);
        }
        // A selection within the paragraph and the window; otherwise the IME sees a caret.
        let anchor = &sel.anchor;
        let mut rel = 0;
        if anchor.story == focus.story && anchor.path == focus.path {
            let a = floor(&para.text, anchor.off);
            let between = |x: usize, y: usize| para.text.get(x..y).map_or(0, |t| t.chars().count());
            rel = if a < off { -signed(between(a, off)) } else { signed(between(off, a)) };
            let inside = if rel < 0 { rel.unsigned_abs() <= before.len() } else { rel.unsigned_abs() <= after.len() };
            if !inside {
                rel = 0;
            }
        }
        Some(Self { before, after, anchor: rel })
    }

    /// A dialog's text field, as egui reports it (`caret` and `anchor` in `char`s).
    pub fn of_field(text: &str, anchor: usize, caret: usize) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let caret = caret.min(chars.len());
        let anchor = anchor.min(chars.len());
        let (b, a) = chars.split_at(caret);
        let before: String = b.iter().collect();
        let after: String = a.iter().collect();
        let (before, _) = tail(&before, BEFORE);
        let (after, _) = head(&after, AFTER);
        let mut rel = signed(anchor) - signed(caret);
        if rel < -signed(before.len()) || rel > signed(after.len()) {
            rel = 0;
        }
        Self { before, after, anchor: rel }
    }

    /// An equation's buffer: spaces to delete.
    pub fn sentinel() -> Self {
        Self { before: vec![' '; SENTINEL], after: Vec::new(), anchor: 0 }
    }

    pub fn buf(&self) -> Buf {
        let focus = self.before.len();
        let mut text = self.before.clone();
        text.extend(&self.after);
        let anchor = usize::try_from(signed(focus).saturating_add(self.anchor)).unwrap_or(focus).min(text.len());
        Buf { text, anchor, focus, compose: None }
    }
}

/// Where the keyboard's text goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Document,
    Equation,
    Field,
}

/// What the host asks Android to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Show,
    Hide,
    /// Replace the IME's buffer: text, selection (UTF-16), generation.
    Push {
        text: String,
        anchor: usize,
        focus: usize,
        generation: u32,
    },
}

/// The IME's state and the host's side of the conversation.
#[derive(Default)]
pub struct Ime {
    generation: u32,
    /// Shadows of the recent pushes, the newest last.
    history: VecDeque<(u32, Shadow)>,
    /// The keyboard was asked for and the connection is live.
    active: bool,
    target: Option<Target>,
    /// Frames in a row without a target (the keyboard hides after two).
    idle: u32,
    /// A push is due whatever the comparison says.
    force: bool,
    requests: Vec<Request>,
}

impl Ime {
    fn current(&mut self) -> Option<&mut Shadow> {
        self.history.back_mut().map(|(_, s)| s)
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// A state from the IME, tagged with the push it was based on: the events that apply it.
    pub fn state(&mut self, generation: u32, new: Buf) -> Vec<Event> {
        let Some(target) = self.target else { return Vec::new() };
        let current = self.generation;
        let Some((_, shadow)) = self.history.iter_mut().find(|(g, _)| *g == generation) else {
            // Based on nothing the host knows: start again from the document.
            self.force = true;
            return Vec::new();
        };
        let mut plan = shadow.apply(new);
        if generation != current {
            // The activity is about to replace this buffer with a newer push: what was composed on it is
            // typed now, or the push would drop it.
            if shadow.preedit_active() {
                let commit = shadow.commit_preedit();
                plan.preedit = None;
                plan.clear_preedit = plan.clear_preedit || commit.clear_preedit;
                plan.insert.push_str(&commit.insert);
            }
            self.force = true;
        }
        events(&plan, target)
    }

    /// A key or a tap: an open composition is typed where it is first.
    pub fn commit(&mut self) -> Vec<Event> {
        let Some(target) = self.target else { return Vec::new() };
        let Some(shadow) = self.current() else { return Vec::new() };
        if !shadow.preedit_active() {
            return Vec::new();
        }
        let plan = shadow.commit_preedit();
        self.force = true;
        events(&plan, target)
    }

    /// After each frame: where the keyboard's text would go, the text there, and whether a touch asked for
    /// the keyboard. Decides on showing, hiding and pushing.
    pub fn after_frame(&mut self, target: Option<Target>, fresh: Option<Window>, tapped: bool) {
        let Some(target) = target else {
            self.idle = self.idle.saturating_add(1);
            if self.active && self.idle >= 2 {
                self.active = false;
                self.target = None;
                self.requests.push(Request::Hide);
            }
            return;
        };
        self.idle = 0;
        if self.target != Some(target) {
            self.target = Some(target);
            self.force = true;
        }
        if tapped {
            if !self.active {
                self.force = true;
            }
            self.active = true;
        }
        if !self.active {
            return;
        }
        let fresh = match target {
            Target::Equation => {
                let low = self.history.back().is_none_or(|(_, s)| s.buf.text.len() < SENTINEL / 2 || s.buf.focus != s.buf.text.len());
                if low {
                    self.force = true;
                }
                Some(Window::sentinel())
            }
            _ => fresh,
        };
        let Some(fresh) = fresh else { return };
        let push = match self.history.back() {
            None => true,
            Some(_) if self.force => true,
            Some((_, s)) => {
                let len = s.len_utf16();
                (!s.preedit_active() && (!s.consistent(&fresh) || len > SOFT)) || len > HARD
            }
        };
        if push {
            self.push(fresh.buf());
        }
        if tapped {
            // The keyboard may have been dismissed while the connection stayed: ask again on every tap.
            self.requests.push(Request::Show);
        }
    }

    fn push(&mut self, buf: Buf) {
        self.force = false;
        self.generation = self.generation.wrapping_add(1);
        let (anchor, focus) = (char_to_utf16(&buf.text, buf.anchor), char_to_utf16(&buf.text, buf.focus));
        self.requests.push(Request::Push { text: buf.text.iter().collect(), anchor, focus, generation: self.generation });
        self.history.push_back((self.generation, Shadow::pushed(buf)));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }
}

fn key(key: Key, mods: Modifiers) -> [Event; 2] {
    [
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: mods },
        Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: mods },
    ]
}

/// How many Backspaces delete the last `chars` characters of `before`, as the app counts them: one per
/// character in scripts that delete by character, one per cluster otherwise, one per paragraph break.
fn backspaces(before: &str, chars: usize) -> usize {
    let target = before.char_indices().rev().nth(chars.saturating_sub(1)).map_or(0, |(i, _)| i);
    let (mut off, mut n) = (before.len(), 0usize);
    while off > target && n < chars {
        let c = before.get(..off).and_then(|t| t.chars().next_back());
        off = match c {
            Some(c) if c == '\n' || cluster::backspaces_by_char(c) => off.saturating_sub(c.len_utf8()),
            _ => cluster::prev_cluster(before, off),
        };
        n += 1;
    }
    n
}

/// Deletes forward of `chars` characters from the start of `after`.
fn deletes(after: &str, chars: usize) -> usize {
    let target = after.char_indices().nth(chars).map_or(after.len(), |(i, _)| i);
    let (mut off, mut n) = (0usize, 0usize);
    while off < target && n < chars {
        off = match after.get(off..).and_then(|t| t.chars().next()) {
            Some('\n') => off + 1,
            _ => cluster::next_cluster(after, off),
        };
        n += 1;
    }
    n
}

/// A plan as the events a keyboard would send.
fn events(plan: &Plan, target: Target) -> Vec<Event> {
    let mut out = Vec::new();
    if plan.is_empty() {
        return out;
    }
    if plan.clear_preedit {
        out.push(Event::Ime(ImeEvent::Preedit { text: String::new(), active_range_chars: None }));
    }
    match target {
        Target::Field => {
            if plan.replace_selection && plan.insert.is_empty() && plan.backspace == 0 && plan.delete == 0 {
                out.extend(key(Key::Backspace, Modifiers::NONE));
            }
            if plan.backspace > 0 || plan.delete > 0 {
                out.push(Event::Ime(ImeEvent::DeleteSurrounding { before_chars: plan.backspace, after_chars: plan.delete }));
            }
        }
        Target::Document | Target::Equation => {
            if plan.replace_selection && (plan.insert.is_empty() || plan.backspace > 0 || plan.delete > 0) {
                out.extend(key(Key::Backspace, Modifiers::NONE));
            }
            for _ in 0..backspaces(&plan.before, plan.backspace) {
                out.extend(key(Key::Backspace, Modifiers::NONE));
            }
            for _ in 0..deletes(&plan.after, plan.delete) {
                out.extend(key(Key::Delete, Modifiers::NONE));
            }
        }
    }
    typed(&plan.insert, &mut out);
    if let Some((anchor, focus)) = plan.select {
        caret_moves(anchor, focus, &mut out);
    }
    if let Some(p) = &plan.preedit {
        let n = p.chars().count();
        out.push(Event::Ime(ImeEvent::Preedit { text: p.clone(), active_range_chars: (n > 0).then_some(n..n) }));
    }
    out
}

/// Typed text: runs of ordinary characters together; spaces, punctuation and quotes one at a time (what
/// AutoCorrect, AutoFormat and smart quotes look at); line breaks as Enter, tabs as Tab; other control
/// characters dropped.
fn typed(text: &str, out: &mut Vec<Event>) {
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<Event>| {
        if !run.is_empty() {
            out.push(Event::Text(std::mem::take(run)));
        }
    };
    for c in text.chars() {
        match c {
            '\n' | '\r' | '\u{2029}' => {
                flush(&mut run, out);
                out.extend(key(Key::Enter, Modifiers::NONE));
            }
            '\t' => {
                flush(&mut run, out);
                out.extend(key(Key::Tab, Modifiers::NONE));
            }
            ' ' | ',' | '.' | ';' | ':' | '!' | '?' | '\'' | '"' => {
                flush(&mut run, out);
                out.push(Event::Text(c.to_string()));
            }
            c if c.is_control() => {}
            c => run.push(c),
        }
    }
    flush(&mut run, out);
}

/// The IME moved the caret (or selected): arrow keys from the caret, Shift held for the selection.
fn caret_moves(anchor: isize, focus: isize, out: &mut Vec<Event>) {
    let arrows = |n: isize, mods: Modifiers, out: &mut Vec<Event>| {
        let k = if n < 0 { Key::ArrowLeft } else { Key::ArrowRight };
        for _ in 0..n.unsigned_abs().min(BEFORE + AFTER) {
            out.extend(key(k, mods));
        }
    };
    arrows(anchor, Modifiers::NONE, out);
    arrows(focus.saturating_sub(anchor), Modifiers::SHIFT, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::StoryRef;
    use wordcraft_engine::Session;

    fn buf(text: &str, sel: usize, compose: Option<(usize, usize)>) -> Buf {
        Buf { text: text.chars().collect(), anchor: sel, focus: sel, compose }
    }

    fn sh(text: &str, sel: usize) -> Shadow {
        Shadow::pushed(buf(text, sel, None))
    }

    fn texts(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.clone()),
                Event::Key { key, pressed: true, modifiers, .. } => Some(format!("{}{key:?}", if modifiers.shift { "Shift+" } else { "" })),
                Event::Ime(ImeEvent::Preedit { text, .. }) => Some(format!("[{text}]")),
                Event::Ime(ImeEvent::DeleteSurrounding { before_chars, after_chars }) => Some(format!("del({before_chars},{after_chars})")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn utf16_conversion_clamps_and_rounds_down_inside_surrogates() {
        let t: Vec<char> = "a😀b".chars().collect();
        assert_eq!([0, 1, 2, 3, 4, 99, -1].map(|u| utf16_to_char(&t, u)), [0, 1, 1, 2, 3, 3, 0]);
        assert_eq!([0, 1, 2, 3, 9].map(|c| char_to_utf16(&t, c)), [0, 1, 3, 4, 4]);
        let b = normalize("a😀b", (4, 1), Some((4, 1)));
        assert_eq!((b.anchor, b.focus, b.compose), (3, 1, Some((1, 3))));
        assert_eq!(normalize("ab", (0, 0), Some((1, 1))).compose, None);
        assert_eq!(normalize("", (i64::MAX, i64::MIN), Some((i64::MIN, i64::MAX))), Buf::default());
    }

    #[test]
    fn a_word_composes_then_commits_with_a_space() {
        let mut s = sh("", 0);
        let p = s.apply(buf("h", 1, Some((0, 1))));
        assert_eq!(texts(&events(&p, Target::Document)), ["[h]"]);
        s.apply(buf("he", 2, Some((0, 2))));
        let p = s.apply(buf("hello ", 6, None));
        assert_eq!(texts(&events(&p, Target::Document)), ["[]", "hello", " "], "the space alone, for AutoCorrect");
        assert!(!s.preedit_active());
    }

    #[test]
    fn backspace_inside_a_composition_only_changes_the_preedit() {
        let mut s = sh("", 0);
        s.apply(buf("hel", 3, Some((0, 3))));
        assert_eq!(texts(&events(&s.apply(buf("he", 2, Some((0, 2)))), Target::Document)), ["[he]"]);
        assert_eq!(texts(&events(&s.apply(buf("", 0, None)), Target::Document)), ["[]"]);
    }

    #[test]
    fn backspaces_count_as_the_app_deletes() {
        let mut s = sh("ab\ncd", 3);
        assert_eq!(texts(&events(&s.apply(buf("abcd", 2, None)), Target::Document)), ["Backspace"], "joins paragraphs");
        // An emoji with a skin tone is one Backspace; Devanagari deletes a character at a time.
        let mut s = sh("x👍🏽", 3);
        assert_eq!(texts(&events(&s.apply(normalize("x", (1, 1), None)), Target::Document)), ["Backspace"]);
        let mut s = sh("कि", 2);
        assert_eq!(texts(&events(&s.apply(buf("", 0, None)), Target::Document)), ["Backspace", "Backspace"]);
        let mut s = sh("abcd", 2);
        assert_eq!(texts(&events(&s.apply(buf("abd", 2, None)), Target::Document)), ["Delete"]);
        assert_eq!(texts(&events(&sh("ab", 2).apply(buf("", 0, None)), Target::Field)), ["del(2,0)"]);
    }

    #[test]
    fn suggestions_and_selections() {
        let mut s = sh("teh ", 4);
        assert_eq!(texts(&events(&s.apply(buf("the ", 4, None)), Target::Document)), ["Backspace", "Backspace", "Backspace", "he", " "]);
        let mut s = Shadow::pushed(Buf { text: "hello".chars().collect(), anchor: 0, focus: 5, compose: None });
        assert_eq!(texts(&events(&s.apply(buf("J", 1, None)), Target::Document)), ["J"], "typing replaces the selection");
        let mut s = sh("abc", 3);
        assert_eq!(texts(&events(&s.apply(buf("abc", 1, None)), Target::Document)), ["ArrowLeft", "ArrowLeft"]);
        let mut s = sh("abc", 3);
        let p = s.apply(Buf { text: "abc".chars().collect(), anchor: 0, focus: 2, compose: None });
        assert_eq!(texts(&events(&p, Target::Document)), ["ArrowLeft", "ArrowLeft", "ArrowLeft", "Shift+ArrowRight", "Shift+ArrowRight"]);
    }

    #[test]
    fn line_breaks_and_tabs_are_keys() {
        let mut out = Vec::new();
        typed("a\nb\tc\u{7}d", &mut out);
        assert_eq!(texts(&out), ["a", "Enter", "b", "Tab", "cd"]);
    }

    #[test]
    fn cjk_korean_and_japanese() {
        let mut s = sh("", 0);
        s.apply(buf("ni", 2, Some((0, 2))));
        assert_eq!(texts(&events(&s.apply(buf("你", 1, None)), Target::Document)), ["[]", "你"]);
        let mut s = sh("", 0);
        s.apply(buf("한", 1, Some((0, 1))));
        assert_eq!(texts(&events(&s.apply(buf("한ㄱ", 2, Some((1, 2)))), Target::Document)), ["[]", "한", "[ㄱ]"]);
        let mut s = sh("", 0);
        s.apply(buf("にほんご", 4, Some((0, 4))));
        assert!(s.apply(buf("にほんご", 2, Some((0, 4)))).is_empty());
    }

    #[test]
    fn a_marked_word_stays_until_it_changes() {
        let mut s = sh("say hello", 9);
        assert!(s.apply(buf("say hello", 9, Some((4, 9)))).is_empty());
        assert_eq!(texts(&events(&s.apply(buf("say hellos", 10, Some((4, 10)))), Target::Document)), ["[s]"]);
    }

    fn session(text: &str) -> Session {
        let mut s = Session::new(Document::new());
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                s.run("text.newParagraph", &serde_json::json!({})).unwrap();
            }
            if !line.is_empty() {
                s.run("text.insert", &serde_json::json!({"text": line})).unwrap();
            }
        }
        s
    }

    #[test]
    fn the_window_shows_the_paragraphs_around_the_caret() {
        let s = session("First line\nSecond 😀 line\nThird");
        let mut sel = s.sel.clone();
        sel.focus.path = sel.focus.path.with_last(1);
        sel.focus.off = "Second ".len();
        sel.anchor = sel.focus.clone();
        let w = Window::of_document(&s.doc, &sel).unwrap();
        assert_eq!(w.before.iter().collect::<String>(), "First line\nSecond ");
        assert_eq!(w.after.iter().collect::<String>(), "😀 line\nThird");
        assert_eq!(w.anchor, 0);
        // A selection inside the paragraph; one reaching elsewhere shows as a caret.
        sel.anchor.off = 0;
        assert_eq!(Window::of_document(&s.doc, &sel).unwrap().anchor, -7);
        sel.anchor.path = sel.anchor.path.with_last(0);
        assert_eq!(Window::of_document(&s.doc, &sel).unwrap().anchor, 0);
        // A long paragraph is cut, never inside a surrogate pair.
        let long = session(&"😀".repeat(2000));
        let w = Window::of_document(&long.doc, &long.sel).unwrap();
        assert!(utf16_len(&w.before) <= BEFORE && w.before.iter().all(|c| *c == '😀'));
        assert!(
            Window::of_document(&long.doc, &Selection::default()).is_some()
                || long.doc.para(StoryRef::Body, &Selection::default().focus.path).is_none()
        );
    }

    #[test]
    fn pushes_follow_the_document_and_echoes_are_quiet() {
        let s = session("teh");
        let mut ime = Ime::default();
        // No target, no keyboard.
        ime.after_frame(None, None, false);
        assert!(ime.take_requests().is_empty());
        let w = Window::of_document(&s.doc, &s.sel);
        ime.after_frame(Some(Target::Document), w.clone(), true);
        let r = ime.take_requests();
        assert!(matches!(r.as_slice(), [Request::Push { text, focus: 3, generation: 1, .. }, Request::Show] if text == "teh"), "{r:?}");
        // The IME echoes the push: nothing to do, nothing to push.
        assert!(ime.state(1, buf("teh", 3, Some((0, 3)))).is_empty());
        ime.after_frame(Some(Target::Document), w.clone(), false);
        assert!(ime.take_requests().is_empty());
        // The user types a space; the app's AutoCorrect makes it "the ": one push.
        assert_eq!(texts(&ime.state(1, buf("teh ", 4, None))), [" "]);
        let corrected = Window { before: "the ".chars().collect(), after: vec![], anchor: 0 };
        ime.after_frame(Some(Target::Document), Some(corrected.clone()), false);
        assert!(matches!(ime.take_requests().as_slice(), [Request::Push { generation: 2, .. }]));
        ime.after_frame(Some(Target::Document), Some(corrected), false);
        assert!(ime.take_requests().is_empty());
        // A state on the replaced push: its composition is typed, and a push follows.
        assert_eq!(texts(&ime.state(1, buf("teh c", 5, Some((4, 5))))), ["[]", "c"]);
        // Two frames without a target hide the keyboard.
        ime.after_frame(None, None, false);
        ime.after_frame(None, None, false);
        assert_eq!(ime.take_requests(), [Request::Hide]);
        assert!(!ime.active());
    }

    #[test]
    fn equations_get_a_buffer_of_spaces() {
        let mut ime = Ime::default();
        ime.after_frame(Some(Target::Equation), None, true);
        let r = ime.take_requests();
        assert!(matches!(&r[0], Request::Push { text, .. } if text == "        "));
        assert_eq!(texts(&ime.state(1, buf("       ", 7, None))), ["Backspace"]);
        assert_eq!(texts(&ime.state(1, buf("       x", 8, None))), ["x"]);
    }

    /// Junk states never panic.
    #[test]
    fn hostile_states_never_panic() {
        let texts = ["", "a", "😀", "a😀b\n", "한ㄱ", "\u{FFFC}x", "\u{0301}e", "कि"];
        let nums = [i64::MIN, -1, 0, 1, 2, 3, 5, 1000, i64::MAX];
        let mut ime = Ime::default();
        ime.after_frame(Some(Target::Document), Some(Window { before: vec!['a'], after: vec![], anchor: 0 }), true);
        let mut n: u64 = 1;
        for i in 0..20_000u32 {
            n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let t = texts[(n >> 33) as usize % texts.len()].repeat(((n >> 40) % 4) as usize);
            let k = |sh: u32| nums[((n >> sh) % nums.len() as u64) as usize];
            let comp = if n & 1 == 0 { None } else { Some((k(3), k(7))) };
            let generation = u32::try_from((n >> 50) % 3).unwrap();
            let _ = ime.state(generation, normalize(&t, (k(11), k(17)), comp));
            if i % 7 == 0 {
                let _ = ime.commit();
                ime.after_frame(
                    Some(Target::Document),
                    Some(Window { before: t.chars().collect(), after: vec![], anchor: k(5).clamp(-3, 3) as isize }),
                    n & 2 == 0,
                );
                let _ = ime.take_requests();
            }
        }
    }
}
