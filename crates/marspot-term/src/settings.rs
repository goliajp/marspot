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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reclaim_enabled: true,
            reclaim_idle_minutes: 30,
            reclaim_prefetch: true,
            appearance_circled_wide: false,
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
pub fn get() -> Arc<Settings> {
    Arc::clone(&cache().read().unwrap_or_else(|p| p.into_inner()).settings)
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
    c.settings = parsed;
    changed
}

/// Install a value directly — for tests, and for callers that already
/// hold the settings they want in force.
#[doc(hidden)]
pub fn set_for_test(s: Settings) {
    let mut c = cache().write().unwrap_or_else(|p| p.into_inner());
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
];

fn value_of(s: &Settings, key: &str) -> String {
    match key {
        "reclaim.enabled" => s.reclaim_enabled.to_string(),
        "reclaim.idle_minutes" => s.reclaim_idle_minutes.to_string(),
        "reclaim.prefetch_on_return" => s.reclaim_prefetch.to_string(),
        "appearance.circled_wide" => s.appearance_circled_wide.to_string(),
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
            // Anything else is a key this build does not know.  Left
            // alone here and preserved verbatim by `render` — a newer
            // marspot's settings must survive an older one reading
            // them.
            _ => {}
        }
    }
    s
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
