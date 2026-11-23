//! What is known of one helper's first screens: the keys one dialog has
//! had, whether the agent has drawn anything, and the marks that stay in
//! the helper's directory when the shell starts over.

use std::io::ErrorKind;
use std::path::Path;

use super::dialogs::{Ask, Dialog, Press};

/// Moves of the cursor in one dialog at most.
const MAX_ARROWS: usize = 6;

/// The keys one dialog has had.
#[derive(Debug, Default)]
pub(super) struct Keys {
    /// The choice the cursor was on when the last arrow went.
    arrow_from: Option<usize>,
    arrows: usize,
    over: bool,
}

impl Keys {
    /// The next key for `d` as it is drawn now.  None: no key goes.  An
    /// arrow or an Enter not taken yet would otherwise be followed by
    /// one more, which lands on whatever comes next.
    pub fn next(&mut self, d: &Dialog) -> Option<Press> {
        if self.over || self.arrow_from == Some(d.cursor) {
            return None;
        }
        let press = d.press();
        if press == Press::Enter {
            self.over = true;
        } else if self.arrows == MAX_ARROWS {
            return None;
        } else {
            self.arrow_from = Some(d.cursor);
            self.arrows += 1;
        }
        Some(press)
    }

    /// No key goes to it any more.
    pub fn end(&mut self) {
        self.over = true;
    }

    pub fn ended(&self) -> bool {
        self.over
    }
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The row the start line's last echo ends on.
fn echo_end(rows: &[&str], echo: &str) -> Option<usize> {
    let mut all = String::new();
    let mut ends = Vec::with_capacity(rows.len());
    for r in rows {
        all.push_str(&squash(r));
        ends.push(all.len());
    }
    let echo = squash(echo);
    let end = all.rfind(&echo)? + echo.len();
    ends.iter().position(|e| *e >= end)
}

/// Whether the agent drew something: text under `echo`, the line that
/// started it, that is not already in `before`.
pub(super) fn drawn(screen: &str, echo: &str, before: &str) -> bool {
    let rows: Vec<&str> = screen.lines().collect();
    // the rest of the echo's row may be the shell's right-hand prompt
    let first = echo_end(&rows, echo).map_or(0, |r| r + 1);
    let new = |r: &str| !r.is_empty() && !before.lines().any(|b| b.trim() == r);
    rows[first..].iter().any(|r| new(r.trim()))
}

/// Take the one Enter the `ask` dialog of the helper at `dir` ever gets;
/// false when it was taken before, by an earlier shell too.
pub(super) fn take_enter(dir: &Path, ask: Ask) -> bool {
    let mark = dir.join(format!("entered-{}", ask.word()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(mark) {
        Ok(_) => true,
        Err(e) => {
            if e.kind() != ErrorKind::AlreadyExists {
                marspot::lx_warn!("helpers.mark_failed", &format!("{}: {e}", dir.display()));
            }
            false
        }
    }
}

/// Note that the parent is told of `d`; false when it is the dialog it
/// was last told of.
pub(super) fn take_told(dir: &Path, d: &Dialog) -> bool {
    let key = format!("{}\n{}\n{}\n", d.ask.word(), d.dirs_text(), d.options.join("\n"));
    let mark = dir.join("told-dialog");
    if std::fs::read_to_string(&mark).is_ok_and(|was| was == key) {
        return false;
    }
    if let Err(e) = std::fs::write(&mark, key) {
        marspot::lx_warn!("helpers.mark_failed", &format!("{}: {e}", dir.display()));
    }
    true
}
