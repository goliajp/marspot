//! C2 — pane-attached search bar overlay.  Owns the user-facing
//! query state (text, case toggle, cursor position, IME preedit) and
//! the input-handling FSM for the editor.  Pure data + key-handling
//! logic — no rendering yet (the renderer hook lands when C5 wires
//! Cmd+F to instantiate one of these via the `PaneTool` trait).
//!
//! The struct intentionally does **not** carry the live "hits"
//! collection — that lives in C3's `SearchList`.  Bar + List get
//! grouped into a `PaneSearch` super-struct (added in C5 along with
//! the keybinding glue); keeping them separate here lets C2's tests
//! exercise input handling in isolation and lets C3 evolve the list
//! shape independently.
//!
//! Spec: `docs/scrollback-search.md` §6.4 + §6.6.5 + §6.7 + §6.8 +
//! §12.C2.

use std::time::{Duration, Instant};

use marspot_term::input_core::{LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::render::{InputDisposition, PaneTool, ToolSlot};

/// 100 ms typing-quiescence window — after this many ms with no
/// further keystrokes, the L2 main loop reads `debounce_until` and
/// emits a fresh `SearchScrollback` frame.  Spec §6.9.
pub const QUERY_DEBOUNCE: Duration = Duration::from_millis(100);

/// Hard cap on query length.  §6.9 zero-result row says "query >
/// 256 chars: clip query rendering at 256; SearchScrollback still
/// encodes whole query (bounded by 1024 cap)".  We enforce the
/// 1024-char hard cap here; rendering clip belongs to the renderer.
pub const QUERY_MAX_CHARS: usize = 1024;

/// Per-pane search bar state.  The `query` and `cursor` are kept in
/// **char** units, not bytes, so insertion / deletion are UTF-8 safe
/// without slicing into multi-byte sequences.
#[derive(Clone, Debug)]
pub struct SearchBar {
    /// The user's typed query.  Re-search debounces 100 ms after the
    /// last keystroke.
    pub query: String,
    /// `[Aa]` toggle.  Default off (browser convention).
    pub case_sensitive: bool,
    /// Monotonic id for the next `SearchScrollback` frame.  L3
    /// auto-cancels in-flight workers when a new query_id arrives
    /// (last-write-wins; D15).
    pub query_id: u32,
    /// Bar is the active input target — printable keys go here
    /// instead of the PTY.  False = closed; main loop short-circuits
    /// `on_key` to `Pass`.
    pub focused: bool,
    /// (focused-1-based, total-hits-known-so-far).  Drawn as
    /// `23/412`.  `None` when query is empty.
    pub counter: Option<(u32, u32)>,
    /// Edit-cursor position in **chars** (`0..=query.chars().count()`).
    pub cursor: usize,
    /// Active IME composition (preedit) — drawn above the query row,
    /// not inserted into `query` until commit.  Empty when not
    /// composing.
    pub preedit: String,
    /// When set in the future, the main loop emits a new
    /// `SearchScrollback` frame.  `None` = no pending re-search.
    /// Reset on every keystroke that mutates `query`.
    pub debounce_until: Option<Instant>,
    /// Pixel rect of the bar inside the pane (set by the renderer
    /// on each frame; consulted by `hit_test`).  All zero until the
    /// first render — that's fine for C2 because `hit_test` is only
    /// called after a real frame has been drawn.
    pub rect_px: ToolRectPx,
}

/// Pixel rect of a tool overlay inside its pane.  The renderer
/// writes this on each frame so input dispatch can hit-test mouse
/// events before they reach the PTY.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ToolRectPx {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Default for SearchBar {
    fn default() -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            query_id: 0,
            focused: false,
            counter: None,
            cursor: 0,
            preedit: String::new(),
            debounce_until: None,
            rect_px: ToolRectPx::default(),
        }
    }
}

impl SearchBar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Char count of `query` — convenience around `chars().count()`.
    pub fn query_char_len(&self) -> usize {
        self.query.chars().count()
    }

    /// Convert a char-cursor (0..=char_len) to a byte index into
    /// `self.query`.  Used by insert/delete helpers + the renderer's
    /// cursor placement.
    pub fn cursor_byte_offset(&self) -> usize {
        let target = self.cursor;
        let mut b = 0;
        for (i, c) in self.query.chars().enumerate() {
            if i == target {
                return b;
            }
            b += c.len_utf8();
        }
        // Cursor at end.
        self.query.len()
    }

    fn schedule_debounce(&mut self, now: Instant) {
        self.debounce_until = Some(now + QUERY_DEBOUNCE);
    }

    fn insert_chars(&mut self, s: &str, now: Instant) -> bool {
        if s.is_empty() {
            return false;
        }
        // Strip non-printables per §6.9 (NUL → space; control chars dropped).
        let mut cleaned = String::with_capacity(s.len());
        for c in s.chars() {
            if c == '\0' {
                cleaned.push(' ');
            } else if c.is_control() {
                // skip
            } else {
                cleaned.push(c);
            }
        }
        if cleaned.is_empty() {
            return false;
        }
        // Cap.
        let have = self.query_char_len();
        let want = cleaned.chars().count();
        if have + want > QUERY_MAX_CHARS {
            let allow = QUERY_MAX_CHARS.saturating_sub(have);
            if allow == 0 {
                return false;
            }
            cleaned = cleaned.chars().take(allow).collect();
        }
        let byte_off = self.cursor_byte_offset();
        self.query.insert_str(byte_off, &cleaned);
        self.cursor += cleaned.chars().count();
        self.schedule_debounce(now);
        true
    }

    fn backspace(&mut self, now: Instant) -> bool {
        if self.cursor == 0 {
            return false;
        }
        // Find byte range of the char left of cursor.
        let mut prev_byte = 0;
        let mut at_byte = 0;
        let mut i = 0;
        for (idx, c) in self.query.char_indices() {
            if i == self.cursor - 1 {
                prev_byte = idx;
                at_byte = idx + c.len_utf8();
                break;
            }
            i += 1;
            // Loop runs to find the (cursor - 1)-th char.
            let _ = idx;
        }
        // Fallback — we should always find it because cursor <= char_len.
        if at_byte == 0 {
            // edge case: cursor at end
            if let Some((idx, c)) = self.query.char_indices().rev().next() {
                prev_byte = idx;
                at_byte = idx + c.len_utf8();
            }
        }
        self.query.replace_range(prev_byte..at_byte, "");
        self.cursor -= 1;
        self.schedule_debounce(now);
        true
    }

    fn delete_forward(&mut self, now: Instant) -> bool {
        let len = self.query_char_len();
        if self.cursor >= len {
            return false;
        }
        let byte_off = self.cursor_byte_offset();
        // Find end of char at cursor.
        let mut end = byte_off;
        if let Some(c) = self.query[byte_off..].chars().next() {
            end = byte_off + c.len_utf8();
        }
        self.query.replace_range(byte_off..end, "");
        self.schedule_debounce(now);
        true
    }

    fn move_cursor_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }
    fn move_cursor_right(&mut self) -> bool {
        let len = self.query_char_len();
        if self.cursor >= len {
            return false;
        }
        self.cursor += 1;
        true
    }
    fn move_cursor_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }
    fn move_cursor_end(&mut self) -> bool {
        let len = self.query_char_len();
        if self.cursor == len {
            return false;
        }
        self.cursor = len;
        true
    }

    /// Reset the bar to closed/empty state — called by C5's Cmd+F
    /// intercept when Esc closes the overlay.
    pub fn close(&mut self) {
        self.focused = false;
        self.query.clear();
        self.cursor = 0;
        self.preedit.clear();
        self.counter = None;
        self.debounce_until = None;
    }

    /// `on_key` body factored out so tests can drive it with a
    /// deterministic `now` (no implicit `Instant::now()` ⇒ no
    /// flakiness).
    pub fn handle_key(
        &mut self,
        ev: &MarspotKeyEvent,
        mods: Modifiers,
        now: Instant,
    ) -> InputDisposition {
        if !self.focused {
            return InputDisposition::Pass;
        }
        // Cmd-Q / Cmd-B etc. (global) — only intercept when we
        // genuinely own the key.  Anything Cmd that ISN'T a search-
        // editor binding falls through.
        if mods.super_key() {
            // Cmd+A: select-all (we don't model selection inside the
            // query; treat as "place cursor at end + mark all
            // selected" — for C2 just move to end).
            if let LogicalKey::Char('a') = ev.logical {
                let _ = self.move_cursor_end();
                return InputDisposition::HandledRequestRedraw;
            }
            // Cmd+V: paste — handled by L2 main on pasteboard read.
            // Cmd+C: copy — L2 handles; default Pass means it falls
            // through to existing grid-selection copy per §6.7.
            return InputDisposition::Pass;
        }
        match &ev.logical {
            LogicalKey::Named(NamedKey::Escape) => {
                // Caller (C5) owns the close decision (clears tools
                // vec).  We just signal "I want to be closed".
                self.close();
                InputDisposition::HandledRequestRedraw
            }
            LogicalKey::Named(NamedKey::Backspace) => {
                if self.backspace(now) {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::Delete) => {
                if self.delete_forward(now) {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::ArrowLeft) => {
                if self.move_cursor_left() {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::ArrowRight) => {
                if self.move_cursor_right() {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::Home) => {
                if self.move_cursor_home() {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::End) => {
                if self.move_cursor_end() {
                    InputDisposition::HandledRequestRedraw
                } else {
                    InputDisposition::Handled
                }
            }
            LogicalKey::Named(NamedKey::Enter) => {
                // Enter = "jump to focused result" — handled by C3's
                // SearchList in its on_key.  Here we don't insert a
                // newline (single-line edit).
                InputDisposition::Pass
            }
            LogicalKey::Named(NamedKey::ArrowUp) | LogicalKey::Named(NamedKey::ArrowDown) => {
                // Up/Down = result selection (§6.7).  Handled by
                // SearchList; bar passes through.
                InputDisposition::Pass
            }
            _ => {
                // Printable char insertion via ev.text (already
                // case-resolved by the window backend).
                if let Some(t) = &ev.text {
                    if self.insert_chars(t, now) {
                        return InputDisposition::HandledRequestRedraw;
                    }
                }
                InputDisposition::Handled
            }
        }
    }

    /// True if `(x, y)` (window-physical px) lies inside the bar's
    /// rect_px.  Used by C5's mouse_down to route clicks.
    pub fn hit_test(&self, x: f64, y: f64) -> bool {
        let r = &self.rect_px;
        if r.w <= 0.0 || r.h <= 0.0 {
            return false;
        }
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
    }
}

impl PaneTool for SearchBar {
    fn slot(&self) -> ToolSlot {
        ToolSlot::Overlay
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        SearchBar::hit_test(self, x, y)
    }
    fn on_key(&mut self, ev: &MarspotKeyEvent, mods: Modifiers) -> InputDisposition {
        self.handle_key(ev, mods, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::input_core::KeyState;

    fn pressed_char(c: char) -> MarspotKeyEvent {
        MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char(c),
            text: Some(c.to_string()),
        }
    }

    fn pressed_named(n: NamedKey) -> MarspotKeyEvent {
        MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Named(n),
            text: None,
        }
    }

    fn focused_bar() -> SearchBar {
        let mut b = SearchBar::new();
        b.focused = true;
        b
    }

    #[test]
    fn c2_default_is_unfocused_and_pass() {
        let mut b = SearchBar::new();
        let ev = pressed_char('a');
        let d = b.handle_key(&ev, Modifiers::default(), Instant::now());
        assert_eq!(d, InputDisposition::Pass);
        assert_eq!(b.query, "");
    }

    #[test]
    fn c2_printable_char_inserts_at_cursor_and_debounces() {
        let mut b = focused_bar();
        let t0 = Instant::now();
        let _ = b.handle_key(&pressed_char('f'), Modifiers::default(), t0);
        let _ = b.handle_key(&pressed_char('o'), Modifiers::default(), t0);
        let _ = b.handle_key(&pressed_char('o'), Modifiers::default(), t0);
        assert_eq!(b.query, "foo");
        assert_eq!(b.cursor, 3);
        assert!(b.debounce_until.is_some());
        let target = b.debounce_until.unwrap();
        assert!(target >= t0 + QUERY_DEBOUNCE - Duration::from_millis(1));
    }

    #[test]
    fn c2_backspace_removes_char_left_of_cursor() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('a'), Modifiers::default(), t);
        b.handle_key(&pressed_char('b'), Modifiers::default(), t);
        b.handle_key(&pressed_char('c'), Modifiers::default(), t);
        assert_eq!(b.query, "abc");
        b.handle_key(&pressed_named(NamedKey::Backspace), Modifiers::default(), t);
        assert_eq!(b.query, "ab");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn c2_backspace_at_zero_is_handled_but_no_change() {
        let mut b = focused_bar();
        let d = b.handle_key(
            &pressed_named(NamedKey::Backspace),
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(d, InputDisposition::Handled);
        assert_eq!(b.query, "");
    }

    #[test]
    fn c2_cursor_arrows_move_correctly() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('a'), Modifiers::default(), t);
        b.handle_key(&pressed_char('b'), Modifiers::default(), t);
        assert_eq!(b.cursor, 2);
        b.handle_key(&pressed_named(NamedKey::ArrowLeft), Modifiers::default(), t);
        assert_eq!(b.cursor, 1);
        b.handle_key(&pressed_named(NamedKey::ArrowLeft), Modifiers::default(), t);
        assert_eq!(b.cursor, 0);
        // No-op at left edge.
        let d = b.handle_key(&pressed_named(NamedKey::ArrowLeft), Modifiers::default(), t);
        assert_eq!(d, InputDisposition::Handled);
        assert_eq!(b.cursor, 0);
        // End jumps to len.
        b.handle_key(&pressed_named(NamedKey::End), Modifiers::default(), t);
        assert_eq!(b.cursor, 2);
        // Home jumps to 0.
        b.handle_key(&pressed_named(NamedKey::Home), Modifiers::default(), t);
        assert_eq!(b.cursor, 0);
        // Insert at home, verify mid-string insertion.
        b.handle_key(&pressed_char('X'), Modifiers::default(), t);
        assert_eq!(b.query, "Xab");
        assert_eq!(b.cursor, 1);
    }

    #[test]
    fn c2_delete_removes_char_right_of_cursor() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('a'), Modifiers::default(), t);
        b.handle_key(&pressed_char('b'), Modifiers::default(), t);
        b.handle_key(&pressed_named(NamedKey::Home), Modifiers::default(), t);
        b.handle_key(&pressed_named(NamedKey::Delete), Modifiers::default(), t);
        assert_eq!(b.query, "b");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn c2_cjk_chars_insert_correctly() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('中'), Modifiers::default(), t);
        b.handle_key(&pressed_char('文'), Modifiers::default(), t);
        assert_eq!(b.query, "中文");
        assert_eq!(b.cursor, 2);
        b.handle_key(&pressed_named(NamedKey::Backspace), Modifiers::default(), t);
        assert_eq!(b.query, "中");
        assert_eq!(b.cursor, 1);
    }

    #[test]
    fn c2_control_chars_stripped_on_insert() {
        let mut b = focused_bar();
        let t = Instant::now();
        // Construct an ev with text containing a NUL + a control char + 'x'.
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char('x'),
            text: Some("\0\u{1b}x".to_string()),
        };
        b.handle_key(&ev, Modifiers::default(), t);
        // NUL becomes ' '; ESC dropped; 'x' kept → " x".
        assert_eq!(b.query, " x");
    }

    #[test]
    fn c2_query_max_chars_enforced() {
        let mut b = focused_bar();
        let t = Instant::now();
        // Insert 1024 chars; should accept all.
        let big = "a".repeat(QUERY_MAX_CHARS);
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char('a'),
            text: Some(big),
        };
        b.handle_key(&ev, Modifiers::default(), t);
        assert_eq!(b.query.len(), QUERY_MAX_CHARS);
        // Try to insert 1 more → no change.
        let pre = b.query.clone();
        b.handle_key(&pressed_char('b'), Modifiers::default(), t);
        assert_eq!(b.query, pre);
    }

    #[test]
    fn c2_escape_closes_bar() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('q'), Modifiers::default(), t);
        assert_eq!(b.query, "q");
        b.handle_key(&pressed_named(NamedKey::Escape), Modifiers::default(), t);
        assert!(!b.focused);
        assert_eq!(b.query, "");
        assert_eq!(b.cursor, 0);
        assert!(b.debounce_until.is_none());
    }

    #[test]
    fn c2_cmd_a_moves_cursor_to_end() {
        let mut b = focused_bar();
        let t = Instant::now();
        b.handle_key(&pressed_char('a'), Modifiers::default(), t);
        b.handle_key(&pressed_char('b'), Modifiers::default(), t);
        b.handle_key(&pressed_named(NamedKey::Home), Modifiers::default(), t);
        assert_eq!(b.cursor, 0);
        let mut cmd = Modifiers::default();
        cmd.super_ = true;
        b.handle_key(&pressed_char('a'), cmd, t);
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn c2_other_cmd_keys_pass_through() {
        let mut b = focused_bar();
        let t = Instant::now();
        let mut cmd = Modifiers::default();
        cmd.super_ = true;
        // Cmd+C — not handled by SearchBar, falls through.
        let d = b.handle_key(&pressed_char('c'), cmd, t);
        assert_eq!(d, InputDisposition::Pass);
    }

    #[test]
    fn c2_hit_test_inside_outside_rect() {
        let mut b = SearchBar::new();
        b.rect_px = ToolRectPx {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 40.0,
        };
        assert!(b.hit_test(150.0, 60.0));
        assert!(b.hit_test(100.0, 50.0)); // top-left corner inclusive
        assert!(!b.hit_test(300.0, 60.0)); // x at right edge exclusive
        assert!(!b.hit_test(50.0, 60.0));
        assert!(!b.hit_test(150.0, 30.0));
        assert!(!b.hit_test(150.0, 90.0));
    }

    #[test]
    fn c2_panetool_trait_routes_through_handle_key() {
        // PaneTool::on_key uses `Instant::now()` internally; we
        // just verify the trait dispatch reaches `handle_key` and
        // changes state.
        let mut b = focused_bar();
        let dyn_tool: &mut dyn PaneTool = &mut b;
        let d = dyn_tool.on_key(&pressed_char('z'), Modifiers::default());
        assert_eq!(d, InputDisposition::HandledRequestRedraw);
        assert_eq!(b.query, "z");
    }
}
