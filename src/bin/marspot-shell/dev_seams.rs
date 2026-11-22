//! Dev seams that tests arm through the environment to drive the real
//! close, quit and badge-menu paths.

use super::*;

impl ShellApp {
    /// Dev seam (`MARSPOT_DEV_CLOSE_EXTRA`): close this window again,
    /// now that it has a painted pair on screen, through the very path
    /// the red button takes.  Hooked here rather than in `redraw`
    /// because a background window may never get a draw callback at
    /// all — the pump is poke-driven, and the test's app is not
    /// frontmost.
    pub(super) fn dev_close_extra_if_armed(&mut self, i: usize) {
        if !self.windows[i].dev_close_after_paint {
            return;
        }
        self.windows[i].dev_close_after_paint = false;
        let window_id = self.windows[i].window_id;
        lx_event!(
            "DEV_CLOSE_EXTRA",
            "pressing this window's close button",
            window_id = window_id
        );
        // `performClose:`, not our own close path: the delegate then
        // calls back into `close_requested` while AppKit is still
        // inside its close machinery, which is the shape the red
        // button has and the shape that crashed.
        marspot::app::perform_close(window_id);
    }

    /// Dev seam (`MARSPOT_DEV_CLOSE_SEQUENCE`): work through the list,
    /// one red button every couple of seconds, starting once every
    /// window named in it has painted.  The last entry is the last
    /// window, so the sequence ends in a real quit.
    /// Dev seam — one badge-menu pick, through the same dispatch the
    /// pointer drives.
    ///
    /// `<sid> <tag>` in the file `MARSPOT_DEV_BADGE_MENU` names; the
    /// file is removed before the pick, so a test that wants two picks
    /// writes it twice and each one happens once.  A malformed line is
    /// dropped with a warning rather than retried — a test that wrote
    /// nonsense should fail on its assertion, not spin here.
    pub(super) fn dev_drive_badge_menu(&mut self) {
        let Some(path) = self.dev_badge_menu_file.clone() else {
            return;
        };
        let Ok(body) = std::fs::read_to_string(&path) else {
            return;
        };
        // Empty means not ready, not malformed: a plain `>` redirect
        // creates the file before it writes, and this tick can land in
        // between.  Deleting it there threw the pick away and the only
        // trace was one `unparsed` warning.  A writer that wants to be
        // read in one piece renames the file into place; either way,
        // nothing is consumed until it parses.
        if body.trim().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&path);
        let mut it = body.split_whitespace();
        let parsed = it
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .zip(it.next().and_then(|s| s.parse::<u32>().ok()));
        let Some((sid, tag)) = parsed else {
            lx_warn!(
                "shell.dev.badge_menu_unparsed",
                &format!("{path:?} is not `<sid> <tag>`: {body:?}")
            );
            return;
        };
        lx_event!(
            "DEV_BADGE_MENU",
            "picking a badge menu entry (dev seam)",
            sid = sid,
            tag = tag
        );
        self.plugin_registry
            .dispatch_pane_badge_menu_action(&self.plugin_host, sid, tag);
    }

    /// Dev seam: quit once the expected windows are on screen.
    pub(super) fn dev_drive_quit(&mut self) {
        let Some(want) = self.dev_quit_after_paint else { return };
        let Some(due) = self.dev_quit_at else {
            let painted = self.windows.iter().filter(|w| w.first_frame_ready).count();
            if painted >= want {
                self.dev_quit_at = Some(Instant::now() + Duration::from_secs(3));
            }
            return;
        };
        if Instant::now() < due {
            return;
        }
        let painted = self.windows.iter().filter(|w| w.first_frame_ready).count();
        self.dev_quit_after_paint = None;
        lx_event!(
            "DEV_QUIT_AFTER_PAINT",
            "quitting the way the dock's Quit item does (dev seam)",
            windows = painted as u64
        );
        marspot::app::request_terminate();
    }

    pub(super) fn dev_drive_close_sequence(&mut self) {
        if self.dev_close_sequence.is_empty() {
            return;
        }
        let Some(due) = self.dev_close_next_at else {
            let all_up = self.dev_close_sequence.iter().all(|id| {
                self.windows
                    .iter()
                    .any(|w| w.window_id == *id && w.first_frame_ready)
            });
            if all_up {
                self.dev_close_next_at =
                    Some(Instant::now() + Duration::from_secs(2));
            }
            return;
        };
        if Instant::now() < due {
            return;
        }
        let window_id = self.dev_close_sequence.pop_front().unwrap_or(0);
        self.dev_close_next_at = Some(Instant::now() + Duration::from_secs(2));
        lx_event!(
            "DEV_CLOSE_SEQUENCE",
            "pressing this window's close button (dev seam)",
            window_id = window_id,
            remaining = self.dev_close_sequence.len()
        );
        marspot::app::perform_close(window_id);
    }
}
