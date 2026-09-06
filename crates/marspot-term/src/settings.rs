//! The user's settings — one file, read by every layer, written by
//! whoever the user changed them from.
//!
//! ## What belongs here
//!
//! A setting is **a decision marspot has chosen not to make for the
//! user**.  Every entry is an admission that the question has no
//! universally right answer: how long is "idle", is a bigger glyph
//! worth a wrong wrap point.  Anything with a right answer stays a
//! constant in the code — the reclamation rules (never the focused
//! pane, never with work in flight) are correctness, not taste, and
//! putting them here would only invite breaking them.
//!
//! ## Format
//!
//! Line-oriented `key = value`, `#` comments, blank lines kept.  A
//! hand-editable file for a user who edits files, and no dependency
//! for a schema this small (self-build principle).
//!
//! **Rewrites preserve everything they do not understand** — unknown
//! keys, comments, ordering.  Same rule the wire protocols follow, and
//! for the same reason: a downgrade, or a build that predates a key,
//! must not silently delete what it cannot read.
//!
//! ## Reading
//!
//! [`get`] hands out a cheap `Arc` snapshot; callers on a hot path
//! take one per frame rather than per cell.  [`reload_if_changed`] is
//! a `stat` — cheap enough to sit on the once-a-second sweep that
//! already runs, which is what makes editing the file by hand take
//! effect without restarting anything.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// Every user-owned decision, resolved.
///
/// Plain values, not `Option`s: a missing key means the default, and
/// the default is a real answer rather than "unset".
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Take idle claudecode panes down and restore them on return.
    ///
    /// Off means panes keep running for as long as marspot does.
    pub reclaim_enabled: bool,
    /// How long a session must have been idle — measured by the age of
    /// its own transcript, not by terminal quiet (claude writes a
    /// `Checking for updates` line every half hour, which resets the
    /// terminal's clock and never the session's).
    ///
    /// 0 means never, which is the same as `reclaim_enabled = false`
    /// and is accepted so the number alone can express it.
    pub reclaim_idle_minutes: u32,
    /// On coming back to marspot, start waking the parked panes
    /// instead of waiting for the click that wants one.
    pub reclaim_prefetch: bool,
    /// Give the circled family (`①②③ Ⓐ ⓪ ❶`) two cells instead of one.
    ///
    /// Off, and the reason is the panel's own cost line: every other
    /// wcwidth on the machine calls them narrow, so widening moves the
    /// **wrap point** — a paragraph that merely scrolled past one comes
    /// back with characters stranded in the left margin (measured
    /// 2026-08-08, shipped and reverted within the hour).  Left as a
    /// setting rather than deleted because at one cell a run like
    /// `①②③` cannot be drawn at a readable size at all, and which of
    /// the two hurts more is genuinely the user's call.
    pub appearance_circled_wide: bool,
    /// Draw `<u>…</u>` as underlined text instead of showing the tags.
    ///
    /// Not a terminal convention — it is a concession to what the
    /// models on the other end actually emit.  codex prints the tag
    /// literally because it does not render HTML, and the sentence the
    /// model meant to underline arrives wearing its markup (reported
    /// 2026-09-06, with the ruling: "<u></u> 是下划线，你就渲染就好了").
    ///
    /// The cost is real and is why this is a setting: a program that
    /// legitimately prints those three characters — `cat` of an HTML
    /// or JSX file — loses them to the styling.  Turn it off and the
    /// tags come back.
    pub render_u_tags: bool,
    /// How far a pane that is not the one you are in steps back,
    /// as a multiplier on the attention ladder.
    ///
    /// The ladder itself (unfocused → resting → parked) is not a
    /// setting: it says what marspot knows about each pane, and the
    /// order is not the user's to change.  *How loudly it says it* is —
    /// a wide grid of panes wants less, a pair of panes wants more,
    /// and neither answer is wrong.  `0.0` turns it off entirely.
    pub dim_scale: f32,
    /// Multiplier on wheel / trackpad scrolling.
    ///
    /// Not a direction: that one has a right answer, and it is
    /// whichever the user already chose in macOS (`MARSPOT_SCROLL_INVERT`
    /// stays for the machine where it is wrong).  Speed has no right
    /// answer — it depends on the mouse.
    pub scroll_factor: f32,
    /// Let Claude Code tell marspot which model each pane is on, by
    /// registering a status-line hook in Claude Code's own settings.
    ///
    /// Off, and the reason is whose file it is.  Turning it on edits
    /// `~/.claude*/settings.json` — somebody else's configuration —
    /// so it is a thing the user asks for, never a thing installing
    /// marspot does to them.  Off, the badge still names the model:
    /// it reads the session transcript and the pane's own startup
    /// banner.  On, it stops being a turn behind in the cases those
    /// two are silent about — a `/model` switch on a parked pane, and
    /// the moment just after a profile cycle.
    ///
    /// A status line the user already wrote is not taken away; it is
    /// chained, and put back when this goes off again.
    pub cc_statusline_hook: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reclaim_enabled: true,
            reclaim_idle_minutes: 30,
            reclaim_prefetch: true,
            appearance_circled_wide: false,
            render_u_tags: true,
            dim_scale: 1.0,
            scroll_factor: 1.0,
            cc_statusline_hook: false,
        }
    }
}

impl Settings {
    /// Resolved reclamation threshold, or `None` for "never".
    ///
    /// The two ways of saying never — the switch and the zero — settle
    /// here so no caller has to check both.
    pub fn reclaim_after(&self) -> Option<std::time::Duration> {
        (self.reclaim_enabled && self.reclaim_idle_minutes > 0)
            .then(|| std::time::Duration::from_secs(self.reclaim_idle_minutes as u64 * 60))
    }
}

/// `<state>/settings.toml`.
pub fn path() -> PathBuf {
    let base: PathBuf = match std::env::var_os("MARSPOT_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Caches/marspot")
        }
    };
    base.join("settings.toml")
}

struct Cache {
    settings: Arc<Settings>,
    /// Last mtime we parsed.  `None` before the first read, and after
    /// a read that found no file — so creating one later is noticed.
    seen: Option<SystemTime>,
}

fn cache() -> &'static RwLock<Cache> {
    static CACHE: std::sync::OnceLock<RwLock<Cache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        RwLock::new(Cache {
            settings: Arc::new(parse(&std::fs::read_to_string(path()).unwrap_or_default())),
            seen: mtime(),
        })
    })
}

fn mtime() -> Option<SystemTime> {
    std::fs::metadata(path()).ok()?.modified().ok()
}

/// The current settings.  Cheap — clones an `Arc`.
///
/// "Cheap" still means a lock and a refcount, which is fine per frame
/// or per tick and **not** fine per byte.  A value read on a per-byte
/// path gets a mirror like [`circled_wide`] instead.
pub fn get() -> Arc<Settings> {
    Arc::clone(&cache().read().unwrap_or_else(|p| p.into_inner()).settings)
}

/// Per-byte mirror of `appearance.circled_wide`.
///
/// `char_width` runs once per character of everything the terminal
/// parses — the `cat-cjk` bench pushes ~200 MB/s through it — so it
/// cannot take a lock and bump a refcount to answer "how wide".  A
/// relaxed atomic, republished whenever the settings change, is the
/// whole cost.
///
/// 0 = unread, 1 = false, 2 = true.  Three states rather than a bool
/// so the first call still goes through `get()` and picks up a file
/// that was parsed before this mirror existed.
static CIRCLED_WIDE: AtomicU8 = AtomicU8::new(0);

/// `appearance.circled_wide`, safe to call per character.
#[inline]
pub fn circled_wide() -> bool {
    match CIRCLED_WIDE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let v = get().appearance_circled_wide;
            CIRCLED_WIDE.store(1 + v as u8, Ordering::Relaxed);
            v
        }
    }
}

/// Republish the per-byte mirrors.  Called wherever `settings` are
/// installed, so a mirror can never be staler than the snapshot.
fn publish_mirrors(s: &Settings) {
    CIRCLED_WIDE.store(1 + s.appearance_circled_wide as u8, Ordering::Relaxed);
}

/// Re-read the file if it has changed on disk.  Returns true when the
/// resolved settings actually differ, so callers can log a transition
/// rather than a heartbeat.
///
/// One `stat` per call.  Deliberately cheap enough to sit on an
/// existing once-a-second sweep: that is what makes hand-editing the
/// file take effect without restarting a layer.
pub fn reload_if_changed() -> bool {
    let now = mtime();
    {
        let c = cache().read().unwrap_or_else(|p| p.into_inner());
        if c.seen == now {
            return false;
        }
    }
    let parsed = Arc::new(parse(&std::fs::read_to_string(path()).unwrap_or_default()));
    let mut c = cache().write().unwrap_or_else(|p| p.into_inner());
    c.seen = now;
    let changed = *c.settings != *parsed;
    publish_mirrors(&parsed);
    c.settings = parsed;
    changed
}

/// Install a value directly — for tests, and for callers that already
/// hold the settings they want in force.
#[doc(hidden)]
pub fn set_for_test(s: Settings) {
    let mut c = cache().write().unwrap_or_else(|p| p.into_inner());
    publish_mirrors(&s);
    c.settings = Arc::new(s);
    c.seen = None;
}

// ─── file format ─────────────────────────────────────────────────

/// Keys, in the order a fresh file writes them.
const KEYS: &[&str] = &[
    "reclaim.enabled",
    "reclaim.idle_minutes",
    "reclaim.prefetch_on_return",
    "appearance.circled_wide",
    "appearance.dim_scale",
    "input.scroll_factor",
    "claudecode.statusline_hook",
];

fn value_of(s: &Settings, key: &str) -> String {
    match key {
        "reclaim.enabled" => s.reclaim_enabled.to_string(),
        "reclaim.idle_minutes" => s.reclaim_idle_minutes.to_string(),
        "reclaim.prefetch_on_return" => s.reclaim_prefetch.to_string(),
        "appearance.circled_wide" => s.appearance_circled_wide.to_string(),
        "appearance.render_u_tags" => s.render_u_tags.to_string(),
        "appearance.dim_scale" => fmt_f32(s.dim_scale),
        "input.scroll_factor" => fmt_f32(s.scroll_factor),
        "claudecode.statusline_hook" => s.cc_statusline_hook.to_string(),
        _ => String::new(),
    }
}

/// Split one line into `(key, value)`, or `None` for a comment, a
/// blank, or anything that is not an assignment.
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    Some((k.trim(), v.trim().trim_matches('"')))
}

pub fn parse(body: &str) -> Settings {
    let mut s = Settings::default();
    for line in body.lines() {
        let Some((k, v)) = split_kv(line) else { continue };
        match k {
            "reclaim.enabled" => s.reclaim_enabled = parse_bool(v, s.reclaim_enabled),
            "reclaim.idle_minutes" => {
                s.reclaim_idle_minutes = v.parse().unwrap_or(s.reclaim_idle_minutes)
            }
            "reclaim.prefetch_on_return" => s.reclaim_prefetch = parse_bool(v, s.reclaim_prefetch),
            "appearance.circled_wide" => {
                s.appearance_circled_wide = parse_bool(v, s.appearance_circled_wide)
            }
            "appearance.render_u_tags" => {
                s.render_u_tags = parse_bool(v, s.render_u_tags)
            }
            "appearance.dim_scale" => {
                s.dim_scale = parse_f32(v, s.dim_scale, 0.0, 2.0)
            }
            "input.scroll_factor" => {
                s.scroll_factor = parse_f32(v, s.scroll_factor, 0.1, 8.0)
            }
            "claudecode.statusline_hook" => {
                s.cc_statusline_hook = parse_bool(v, s.cc_statusline_hook)
            }
            // Anything else is a key this build does not know.  Left
            // alone here and preserved verbatim by `render` — a newer
            // marspot's settings must survive an older one reading
            // them.
            _ => {}
        }
    }
    s
}

/// A float, clamped to a range the rest of the program can survive.
///
/// Out of range is treated as a typo and ignored rather than clamped:
/// `scroll_factor = 100` is far more likely a slip than a wish, and
/// silently honouring a tenth of it teaches nothing.
fn parse_f32(v: &str, fallback: f32, lo: f32, hi: f32) -> f32 {
    match v.parse::<f32>() {
        Ok(f) if f.is_finite() && f >= lo && f <= hi => f,
        _ => fallback,
    }
}

/// Trailing-zero-free, so `1.0` writes as `1` and a hand-edited file
/// does not grow noise every time the panel rewrites it.
fn fmt_f32(f: f32) -> String {
    let s = format!("{f:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".to_string() } else { s.to_string() }
}

fn parse_bool(v: &str, fallback: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => fallback,
    }
}

const HEADER: &str = "\
# marspot settings.  Edited here or from the toolbar's settings panel;
# either way the change takes effect within a second, with nothing
# restarted.
#
# Only decisions that have no universally right answer live here.  The
# rules that keep reclamation safe (never the focused pane, never with
# work in flight) are not settings — they are correctness.
";

/// The file body for `s`, preserving everything in `existing` that
/// this build does not understand.
///
/// Known keys are rewritten **in place**, so a user's comments and
/// ordering survive a write from the panel.  Keys the file does not
/// have are appended.
pub fn render(s: &Settings, existing: &str) -> String {
    let mut out = String::with_capacity(existing.len() + 256);
    let mut written: Vec<&str> = Vec::new();
    if existing.trim().is_empty() {
        out.push_str(HEADER);
        out.push('\n');
    }
    for line in existing.lines() {
        match split_kv(line) {
            Some((k, _)) if KEYS.contains(&k) => {
                out.push_str(&format!("{k} = {}\n", value_of(s, k)));
                written.push(KEYS.iter().find(|x| **x == k).unwrap());
            }
            // Comments, blanks, and keys from a build that is not this
            // one — verbatim.
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for k in KEYS {
        if !written.contains(k) {
            out.push_str(&format!("{k} = {}\n", value_of(s, k)));
        }
    }
    out
}

/// Atomic write — `.tmp` + rename, as every other state file does.
pub fn write(s: &Settings) -> io::Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existing = std::fs::read_to_string(&p).unwrap_or_default();
    let body = render(s, &existing);
    let mut tmp = p.clone();
    tmp.set_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers go out and come back the same, and a value the panel
    /// has no button for survives being looked at — the file is
    /// hand-editable, so a rewrite must not quietly round it.
    #[test]
    fn floats_survive_the_round_trip_and_a_hand_edited_one_is_kept() {
        let s = Settings { dim_scale: 0.6, scroll_factor: 2.5, ..Settings::default() };
        let back = parse(&render(&s, ""));
        assert_eq!(back, s, "round trip");
        // 1.0 writes without a trailing `.00`.
        assert!(render(&Settings::default(), "").contains("appearance.dim_scale = 1\n"));
        // A value between the buttons is a value.
        let odd = parse("appearance.dim_scale = 0.85\ninput.scroll_factor = 3\n");
        assert!((odd.dim_scale - 0.85).abs() < 1e-6);
        assert!((odd.scroll_factor - 3.0).abs() < 1e-6);
        assert_eq!(parse(&render(&odd, "")), odd, "and it survives a rewrite");
    }

    /// Out of range is a typo, not a wish: honouring a clamped tenth
    /// of `scroll_factor = 100` would leave the user with a setting
    /// they did not ask for and no way to tell why.
    #[test]
    fn a_value_outside_the_range_is_ignored_not_clamped() {
        for body in [
            "input.scroll_factor = 100",
            "input.scroll_factor = 0",
            "input.scroll_factor = -1",
            "input.scroll_factor = fast",
            "appearance.dim_scale = 9",
            "appearance.dim_scale = -0.5",
        ] {
            let s = parse(body);
            assert_eq!(s, Settings::default(), "{body:?} must leave the defaults alone");
        }
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let s = parse("");
        assert_eq!(s, Settings::default());
        assert_eq!(s.reclaim_after().map(|d| d.as_secs()), Some(1800));
    }

    #[test]
    fn values_parse_in_the_spellings_people_use() {
        let s = parse(
            "# a comment\n\
             reclaim.enabled = off\n\
             reclaim.idle_minutes = 120\n\
             reclaim.prefetch_on_return = \"yes\"\n",
        );
        assert!(!s.reclaim_enabled);
        assert_eq!(s.reclaim_idle_minutes, 120);
        assert!(s.reclaim_prefetch);
        assert_eq!(s.reclaim_after(), None, "the switch off means never");

        // Either way of saying never resolves the same, so no caller
        // has to check both.
        let z = parse("reclaim.idle_minutes = 0\n");
        assert!(z.reclaim_enabled);
        assert_eq!(z.reclaim_after(), None);
    }

    /// Garbage keeps the default rather than taking a wild value — a
    /// typo in a hand-edited file must not switch reclamation on for
    /// a user who was turning it off.
    #[test]
    fn an_unparseable_value_keeps_the_default() {
        let s = parse("reclaim.idle_minutes = soon\nreclaim.enabled = maybe\n");
        assert_eq!(s, Settings::default());
    }

    /// The rule every wire reader in this codebase follows, applied to
    /// the settings file: a build that does not understand a key must
    /// not delete it.  Otherwise opening the panel once on an older
    /// marspot silently drops whatever a newer one wrote.
    #[test]
    fn a_rewrite_preserves_comments_order_and_unknown_keys() {
        let existing = "\
# my own note
reclaim.idle_minutes = 15

# a key from a newer build
appearance.font_size = 13
reclaim.enabled = true
";
        let mut s = parse(existing);
        assert_eq!(s.reclaim_idle_minutes, 15);
        s.reclaim_idle_minutes = 45;
        s.reclaim_enabled = false;
        let out = render(&s, existing);

        assert!(out.contains("# my own note"), "comment lost:\n{out}");
        assert!(
            out.contains("appearance.font_size = 13"),
            "unknown key lost — a downgrade would eat it:\n{out}"
        );
        assert!(out.contains("reclaim.idle_minutes = 45"), "{out}");
        assert!(out.contains("reclaim.enabled = false"), "{out}");
        // In place, not appended: the user's ordering is theirs.
        let i_note = out.find("# my own note").unwrap();
        let i_idle = out.find("reclaim.idle_minutes").unwrap();
        let i_unknown = out.find("appearance.font_size").unwrap();
        assert!(i_note < i_idle && i_idle < i_unknown, "reordered:\n{out}");
        // A key the file lacked is appended, not dropped.
        assert!(out.contains("reclaim.prefetch_on_return = "), "{out}");

        // And the round trip is stable.
        assert_eq!(parse(&out), s);
    }

    /// The mirror exists because `char_width` runs per parsed
    /// character; if it can go stale, the terminal draws with one
    /// width and lays out with another.
    #[test]
    fn the_per_byte_mirror_tracks_the_snapshot() {
        set_for_test(Settings { appearance_circled_wide: true, ..Settings::default() });
        assert!(circled_wide());
        assert_eq!(circled_wide(), get().appearance_circled_wide);

        set_for_test(Settings { appearance_circled_wide: false, ..Settings::default() });
        assert!(!circled_wide());
        assert_eq!(circled_wide(), get().appearance_circled_wide);
    }

    #[test]
    fn a_fresh_file_explains_itself() {
        let out = render(&Settings::default(), "");
        assert!(out.starts_with("# marspot settings"), "{out}");
        for k in KEYS {
            assert!(out.contains(k), "missing {k}:\n{out}");
        }
        assert_eq!(parse(&out), Settings::default());
    }
}
