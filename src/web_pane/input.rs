//! What the person does to a web pane, on its way from the main thread
//! to the connection thread, and what the page says back.  The main
//! thread queues input and wakes the thread; the queue is bounded, a
//! scroll joins the scroll before it and a pointer move replaces the
//! move before it, so a fast wheel or a busy pointer cannot grow it.
//! The page's answers (the pointer's shape, copied text, a link to open
//! elsewhere) wait in single slots for the main thread to take.

use std::collections::VecDeque;
use std::time::Instant;

use madcawl_wire::input::{mods, Action, Cursor, Key, Pointer};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

use super::{Shared, WebPane};

/// Input frames waiting for the thread, at most.
pub const QUEUE_CAP: usize = 256;

/// One thing for the connection thread to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Hundredths of a CSS px, and when the first wheel event in it came
    Scroll(i32, i32, Instant),
    Pointer(Pointer),
    Key(Key),
    Copy,
    Paste(String),
    Visible(bool),
    /// A pane of a new size: device pixels, and the scale in hundredths
    Resize((f64, f64), u32),
}

/// What the page said for the main thread.
#[derive(Default, Debug, PartialEq)]
pub struct Said {
    pub cursor: Option<Cursor>,
    pub copied: Option<String>,
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

/// The DOM `key` value of a key, `None` for one the page has no name for.
pub fn dom_key(event: &MarspotKeyEvent) -> Option<String> {
    use NamedKey::*;
    let named = |n: NamedKey| -> &'static str {
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
    };
    match &event.logical {
        LogicalKey::Named(n) => Some(named(*n).to_owned()),
        LogicalKey::Char(c) => Some(event.text.as_ref().map_or_else(|| c.to_string(), |t| t.as_str().to_owned())),
        LogicalKey::Other => event.text.as_ref().map(|t| t.as_str().to_owned()),
    }
    .filter(|k| !k.is_empty() && k.len() <= madcawl_wire::input::MAX_KEY_NAME)
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
        let text = event.text.as_ref().map(|t| t.as_str()).filter(|t| t.len() <= madcawl_wire::input::MAX_KEY_TEXT);
        self.send(Out::Key(Key { key, text: text.unwrap_or("").to_owned(), mods: mods_of(m), repeat: event.repeat }));
    }

    pub fn copy(&mut self) {
        self.send(Out::Copy);
    }

    pub fn paste(&mut self, text: &str) {
        self.send(Out::Paste(text.to_owned()));
    }

    /// Whether the pane is on screen; sent only when it changes.
    pub fn set_visible(&mut self, on: bool) {
        if self.visible != on {
            self.visible = on;
            self.send(Out::Visible(on));
        }
    }

    /// The pane is `px` device pixels now; sent only when that changed.
    pub fn resize_px(&mut self, px: (f64, f64), scale: u32) {
        if self.px != Some((px, scale)) {
            let first = self.px.is_none();
            self.px = Some((px, scale));
            if !first {
                self.send(Out::Resize(px, scale));
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
