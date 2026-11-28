//! What the person does to a web pane, on its way from the main thread
//! to the connection thread, and what the page says back.  The main
//! thread queues input and wakes the thread; the queue is bounded, a
//! scroll joins the scroll before it and a pointer move replaces the
//! move before it, so a fast wheel or a busy pointer cannot grow it.
//! Whether the pane is shown and its size are not queued: each is one
//! slot holding the latest, so a full queue never loses them and the
//! thread makes a surface for the last size only.  The page's answers
//! (the pointer's shape, copied text, a link to open elsewhere) wait in
//! single slots for the main thread to take.

use std::collections::VecDeque;
use std::time::Instant;

use madcawl_wire::input::{clip, mods, Action, Cursor, Pointer, MAX_KEY_NAME, MAX_KEY_TEXT, MAX_TEXT};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

use super::{Shared, WebPane};

/// Input frames waiting for the thread, at most.
pub const QUEUE_CAP: usize = 256;

/// A short string kept inline, so a key press allocates nothing on the
/// main thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Short<const N: usize> {
    len: u8,
    b: [u8; N],
}

impl<const N: usize> Short<N> {
    /// `s`, when it fits.
    pub fn new(s: &str) -> Option<Short<N>> {
        let mut b = [0u8; N];
        b.get_mut(..s.len())?.copy_from_slice(s.as_bytes());
        Some(Short { len: s.len() as u8, b })
    }

    pub fn as_str(&self) -> &str {
        // only ever filled from a whole &str
        std::str::from_utf8(&self.b[..usize::from(self.len)]).unwrap_or_default()
    }
}

/// A key's DOM `key` value: a fixed name, or the text the key types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyName {
    Named(&'static str),
    Typed(Short<MAX_KEY_NAME>),
}

impl KeyName {
    pub fn as_str(&self) -> &str {
        match self {
            KeyName::Named(n) => n,
            KeyName::Typed(t) => t.as_str(),
        }
    }
}

/// A key press for the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyOut {
    pub key: KeyName,
    pub text: Short<MAX_KEY_TEXT>,
    pub mods: u32,
    pub repeat: bool,
}

/// One thing for the connection thread to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Hundredths of a CSS px, and when the first wheel event in it came
    Scroll(i32, i32, Instant),
    Pointer(Pointer),
    Key(KeyOut),
    /// Ask for the selection; the answer names this number
    Copy(u64),
    Paste(String),
}

/// What the page said for the main thread.
#[derive(Default, Debug, PartialEq)]
pub struct Said {
    pub cursor: Option<Cursor>,
    /// The number of the copy it answers, and the text
    pub copied: Option<(u64, String)>,
    pub open: Option<String>,
}

impl Said {
    pub fn is_empty(&self) -> bool {
        self.cursor.is_none() && self.copied.is_none() && self.open.is_none()
    }
}

/// Queue `o`, joining it with the one before it where that loses
/// nothing a page would see.
pub fn push(q: &mut VecDeque<Out>, o: Out) {
    match (q.back_mut(), o) {
        (Some(Out::Scroll(x, y, _)), Out::Scroll(dx, dy, _)) => {
            *x = x.saturating_add(dx);
            *y = y.saturating_add(dy);
        }
        (Some(Out::Pointer(last)), Out::Pointer(p)) if last.action == Action::Move && p.action == Action::Move => *last = p,
        (_, o) => {
            if q.len() == QUEUE_CAP {
                q.pop_front();
            }
            q.push_back(o);
        }
    }
}

fn hundredths(css: f64) -> i32 {
    let max = f64::from(madcawl_wire::input::MAX_PX);
    (css * 100.0).round().clamp(-max, max) as i32
}

pub fn mods_of(m: Modifiers) -> u32 {
    [(m.shift, mods::SHIFT), (m.control, mods::CONTROL), (m.alt, mods::ALT), (m.super_, mods::META)]
        .iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, bit)| acc | bit)
}

fn named(n: NamedKey) -> &'static str {
    use NamedKey::*;
    match n {
        Enter => "Enter",
        Backspace => "Backspace",
        Tab => "Tab",
        Escape => "Escape",
        ArrowUp => "ArrowUp",
        ArrowDown => "ArrowDown",
        ArrowLeft => "ArrowLeft",
        ArrowRight => "ArrowRight",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Home => "Home",
        End => "End",
        Insert => "Insert",
        Delete => "Delete",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
    }
}

/// Whether pasting `text` into a web pane cuts it.
pub fn paste_cut(text: &str) -> bool {
    text.len() > MAX_TEXT
}

/// The DOM `key` value of a key, `None` for one the page has no name for.
pub fn dom_key(event: &MarspotKeyEvent) -> Option<KeyName> {
    let typed = |s: &str| Short::new(s).filter(|_| !s.is_empty()).map(KeyName::Typed);
    match &event.logical {
        LogicalKey::Named(n) => Some(KeyName::Named(named(*n))),
        LogicalKey::Char(c) => match &event.text {
            Some(t) => typed(t.as_str()),
            None => typed(c.encode_utf8(&mut [0u8; 4])),
        },
        LogicalKey::Other => event.text.as_ref().and_then(|t| typed(t.as_str())),
    }
}

impl Shared {
    fn queue(&self, o: Out) {
        push(&mut self.lock().out, o);
    }

    /// The page's answers so far, taken.
    pub(super) fn take_said(&self) -> Said {
        std::mem::take(&mut self.lock().said)
    }
}

impl WebPane {
    fn send(&mut self, o: Out) {
        self.shared.queue(o);
        self.wake();
    }

    /// Scroll by `dx`, `dy` CSS px.
    pub fn scroll(&mut self, dx: f64, dy: f64) {
        let (x, y) = (hundredths(dx), hundredths(dy));
        if x != 0 || y != 0 {
            self.send(Out::Scroll(x, y, Instant::now()));
        }
    }

    /// The pointer at `x`, `y` CSS px of the page.
    pub fn pointer(&mut self, action: Action, x: f64, y: f64, m: Modifiers) {
        self.send(Out::Pointer(Pointer { action, x: hundredths(x), y: hundredths(y), button: 0, mods: mods_of(m) }));
    }

    /// A key, sent when pressed; the page fires its own release.
    pub fn key(&mut self, event: &MarspotKeyEvent, m: Modifiers) {
        if event.state != KeyState::Pressed {
            return;
        }
        let Some(key) = dom_key(event) else { return };
        let text = event.text.as_ref().and_then(|t| Short::new(t.as_str())).unwrap_or(Short { len: 0, b: [0; MAX_KEY_TEXT] });
        self.send(Out::Key(KeyOut { key, text, mods: mods_of(m), repeat: event.repeat }));
    }

    /// Ask the page for its selection; only the answer to this ask goes
    /// to the clipboard.
    pub fn copy(&mut self) {
        self.copy_seq += 1;
        self.awaiting_copy = Some(self.copy_seq);
        self.send(Out::Copy(self.copy_seq));
    }

    /// Whether `seq` answers the copy asked for, which it then uses up.
    pub fn copy_answers(&mut self, seq: u64) -> bool {
        let ok = self.awaiting_copy == Some(seq);
        if ok {
            self.awaiting_copy = None;
        } else {
            self.stray_copies += 1;
        }
        ok
    }

    /// Answers to no copy asked for, so far.
    pub fn stray_copies(&self) -> u64 {
        self.stray_copies
    }

    /// Type `text` into the page, cut to `MAX_TEXT` at a character
    /// boundary (see `paste_cut`).
    pub fn paste(&mut self, text: &str) {
        let kept = clip(text, MAX_TEXT);
        if kept.len() < text.len() {
            crate::lx_warn!("web_pane.paste_cut", "pasted text cut to fit a frame", session = self.id, bytes = text.len(), kept = kept.len());
        }
        self.send(Out::Paste(kept.to_owned()));
    }

    /// A press with Command held may open one link in a pane of its own;
    /// any other press takes that back.
    pub fn allow_open(&mut self, on: bool) {
        self.open_allowed = on;
    }

    /// Whether a link may open now, which uses the permission up.
    pub fn take_open(&mut self) -> bool {
        std::mem::take(&mut self.open_allowed)
    }

    /// Whether the pane is on screen; sent only when it changes.
    pub fn set_visible(&mut self, on: bool) {
        if self.visible != on {
            self.visible = on;
            self.shared.lock().visible = Some(on);
            self.wake();
        }
    }

    /// The pane is `px` device pixels now; sent only when that changed.
    pub fn resize_px(&mut self, px: (f64, f64), scale: u32) {
        if self.px != Some((px, scale)) {
            let first = self.px.is_none();
            self.px = Some((px, scale));
            if !first {
                self.shared.lock().resize = Some((px, scale));
                self.wake();
            }
        }
    }

    /// What the page said since the last call.
    pub fn take_said(&mut self) -> Said {
        self.shared.take_said()
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
