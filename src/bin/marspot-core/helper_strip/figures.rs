//! How a row says how long it has run and what its context holds, in
//! the words Claude Code uses, and how a text is cut to a width.

use super::compose::{Arrow, Row, State};
use super::strip::glyphs;
use std::fmt::Write;

/// A short text kept in place.  What does not fit is left out.
pub(super) struct Text {
    buf: [u8; 64],
    len: usize,
}

impl Text {
    pub(super) const EMPTY: Text = Text { buf: [0; 64], len: 0 };

    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// The pieces that are not empty, with ` · ` between.
    pub(super) fn join(&mut self, piece: &str) {
        if !piece.is_empty() {
            let dot = if self.as_str().is_empty() { "" } else { " · " };
            let _ = write!(self, "{dot}{piece}");
        }
    }
}

impl Write for Text {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let to = self.buf.get_mut(self.len..self.len + s.len()).ok_or(std::fmt::Error)?;
        to.copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

/// Whole seconds below a minute, rounded ones above.
pub(super) fn elapsed(ms: u64) -> Text {
    let mut t = Text::EMPTY;
    let _ = if ms < 60_000 {
        write!(t, "{}s", ms / 1000)
    } else {
        // a carry from the seconds goes all the way up
        let s = (ms + 500) / 1000;
        let (d, h, m, s) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
        match (d, h) {
            (0, 0) => write!(t, "{m}m {s}s"),
            (0, _) => write!(t, "{h}h {m}m {s}s"),
            _ => write!(t, "{d}d {h}h {m}m"),
        }
    };
    t
}

const UNITS: [(u64, char); 4] = [(1_000, 'k'), (1_000_000, 'm'), (1_000_000_000, 'b'), (1_000_000_000_000, 't')];

/// `115.3k`: the number itself below a thousand, one decimal above.
pub(super) fn compact(n: u64) -> Text {
    let mut t = Text::EMPTY;
    let tenths = |unit: u64| (n as u128 * 10 + unit as u128 / 2) / unit as u128;
    let last = UNITS[UNITS.len() - 1].0;
    // 999 950 is a million to one decimal
    let unit = UNITS.iter().find(|(u, _)| tenths(*u) < 10_000 || *u == last).filter(|_| n >= 1000);
    let _ = match unit {
        Some((u, c)) => write!(t, "{}.{}{c}", tenths(*u) / 10, tenths(*u) % 10),
        None => write!(t, "{n}"),
    };
    t
}

pub(super) fn width(text: &str) -> u16 {
    glyphs(text).fold(0u16, |w, (_, g)| w.saturating_add(g))
}

/// `text` in at most `room` cells: what is kept of it, and the ellipsis
/// it ends in if that is not all.
pub(super) fn cut(text: &str, room: u16) -> (&str, &'static str) {
    if width(text) <= room {
        return (text, "");
    }
    if room == 0 {
        return ("", "");
    }
    let (mut w, mut end) = (0, 0);
    for (g, gw) in glyphs(text) {
        if w + gw > room - 1 {
            break;
        }
        w += gw;
        end = g.as_ptr() as usize - text.as_ptr() as usize + g.len();
    }
    (&text[..end], "…")
}

/// The figures of a row: how it ended, for how long it ran, its tokens.
/// How it ended takes no part in how wide the figures' column is.
pub(super) fn figures(row: &Row, ending: bool) -> Text {
    let mut t = Text::EMPTY;
    let waits = match row.state {
        State::Awaiting => Some("awaiting approval"),
        State::Asked => Some("awaiting answer"),
        _ => None,
    };
    if let Some(w) = waits {
        t.join(w);
        return t;
    }
    match row.state {
        State::Failed if ending => t.join("failed"),
        State::Stopped if ending => t.join("stopped"),
        State::Idle if ending => t.join("idle"),
        _ => {}
    }
    t.join(elapsed(row.elapsed_ms).as_str());
    if let Some(n) = row.tokens.filter(|n| *n > 0) {
        let arrow = if row.arrow == Arrow::Up { "↑" } else { "↓" };
        let _ = write!(t, " · {arrow} {} tokens", compact(n).as_str());
    }
    t
}
