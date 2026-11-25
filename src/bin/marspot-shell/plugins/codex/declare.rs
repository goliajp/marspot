//! What a codex pane is declared as, each tick: an agent TUI with no
//! markup rendering and no wheel keys while codex runs, a shell again
//! once it has left.

use std::collections::HashMap;

use super::badge::publish_badge;
use crate::plugins::PluginHost;

pub(super) fn declare(host: &dyn PluginHost, last_badge: &mut HashMap<u64, String>, sid: u64, has_codex: bool, text: &str) {
    if has_codex {
        // Declare how the wheel reaches codex.  Verified by
        // injecting into a real pty: `PageUp` alone changes
        // nothing, `Ctrl+T` opens its /TRANSCRIPT/ view, and
        // `PageUp`/`PageDown` page it from there.
        //
        // Nothing leaves the view: the user asked for the
        // wheel to take them in but never to throw them out,
        // since one stray tick at the bottom would otherwise
        // close what they were reading.  `Esc` stays theirs.
        //
        // `/TRANSCRIPT/` is the rule codex draws across the top
        // while that view is open, and it survives paging —
        // so L2 can read the state off the screen rather than
        // remember it.  That matters because `Ctrl+T` is a
        // TOGGLE: measured, a second one closes the view, so a
        // remembered flag going stale would shut the transcript
        // instead of opening it (2026-09-06: after leaving the
        // view, scrolling could not get back in).
        // codex does not render HTML, so a model that writes
        // `<u>…</u>` has its markup land on screen as text.
        // Declared per pane, never globally: a terminal is
        // where people TALK about markup, and switched on
        // everywhere it ate the tags out of the conversation
        // specifying this (2026-09-06).
        //
        // Re-issued every tick, like the badge and unlike the
        // wheel keys.  This one has to reach L3, and an L3
        // that is mid-execv when it arrives simply drops it —
        // which is exactly what happened the first time it
        // shipped: L1 declared 1.4 s after the session images
        // were swapped, and no pane ever heard it.  Repeating
        // costs one small frame every two seconds and makes
        // the restart window cost a tick instead of forever.
        // `<u>` rendering is NOT declared any more.  It was
        // added because codex printed its own `<u>` markup as
        // text; measured across 102 MB of real codex traffic
        // since, `<u>` appears TWICE (and `</u>` eight times —
        // not even paired), while `<h2>` and `<p>` appear 529
        // times because the model prints HTML documents into
        // the pane.  Swallowing a real document's tags is now
        // 250x more likely than fixing codex's own, so the
        // feature is net-negative here.  `render_u_tags` in
        // settings.toml still turns it on for anyone who wants
        // it everywhere.
        // Re-issued every tick on purpose: a core swap starts
        // L2 with an empty map, and a declaration sent once
        // never reaches it.  That is how a codex pane lost its
        // hard-wrap link merging on 2026-09-07 while the three
        // unwrapped paths beside it still worked.
        let _ = host.set_pane_agent_tui(sid, true);
        let _ = host.set_pane_render_markup(sid, false);
        // The wheel is NOT routed into codex's transcript any
        // more.  It was, because a codex pane had no history
        // of its own to scroll: codex reserves its input box
        // with a scroll region anchored at row 1, and rows
        // leaving the top of a region used to be dropped
        // rather than kept — so the pane's scrollback was
        // empty and the transcript key was the only way back.
        //
        // Grid::scroll_up_region now keeps them, which is what
        // iTerm2 does (measured 2026-09-07 with the identical
        // sequence: all 120 lines stayed reachable).  So the
        // wheel does what it does in every other pane, and
        // what it already did in claudecode — which is the
        // experience this was asked to match.  Ctrl+T is still
        // codex's own key for anyone who wants its transcript.
        // Re-issued every tick, for the same reason the agent-TUI
        // declaration above is: L2's state can outlive L1's.  A
        // one-shot clear kept in `self.declared` only fires for a
        // pane THIS L1 process declared for, so after an L1
        // restart — a silent update, or the cold launch after a
        // reboot — the set is empty, the clear is never sent, and
        // the wheel keys L2 is still holding stay held.  The
        // symptom is codex's Ctrl+T transcript opening on a wheel
        // scroll again, months after that was removed (2026-09-08
        // field report).  L2 drops a repeat clear silently, so the
        // heartbeat costs one small frame per tick per codex pane.
        let _ = host.set_pane_wheel_keys(sid, b"", b"", b"", b"");
        publish_badge(host, last_badge, sid, text);
    } else if last_badge.remove(&sid).is_some() {
        let _ = host.set_pane_agent_tui(sid, false);
        let _ = host.set_pane_wheel_keys(sid, b"", b"", b"", b"");
        // codex is gone; the pane is a shell again, and a
        // shell's `<u>` is somebody's text.
        let _ = host.set_pane_render_markup(sid, false);
        // codex left this pane: clear the badge we set, and
        // only the one we set — another plugin may own it now.
        let _ = host.set_pane_badge(sid, "");
    }
}
