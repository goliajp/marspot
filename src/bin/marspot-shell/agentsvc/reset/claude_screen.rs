//! What the Claude CLI shows around `/limit-reset`, read as text: the
//! account's cached feature flags that switch the weekly session reset
//! on and give its lines, the line printed after the command that says
//! how the claim went, and the date that line names.

use marspot_peer::json::{self, Value};

use super::Res;

/// What the CLI printed on an account not at its five-hour limit
/// (Claude Code 2.1.332, 2026-11-26).
pub(super) const NOT_AVAILABLE: &str = "A 5-hour limit reset isn't available right now.";

/// The command as typed, and as the CLI echoes it above its output.
const COMMAND: &str = "/limit-reset";

/// The weekly session reset as the account's cached flags describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SessionReset {
    pub enabled: bool,
    /// Each with `{date}` where the CLI puts a date.
    pub success: Option<String>,
    pub spent: Option<String>,
    pub unavailable: Option<String>,
}

fn flag<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    ["cachedGrowthBookFeatures", "cachedFeatureFlags"].iter().find_map(|k| v.at(&[k, name]))
}

/// The weekly session reset of the account whose `.claude.json` is
/// `body`; read only, never written.
pub(super) fn session_reset(body: &str) -> SessionReset {
    let Ok(v) = json::parse(body) else { return SessionReset::default() };
    let Some(f) = flag(&v, "tengu_nifty_lemur") else { return SessionReset::default() };
    let line = |k: &str| f.str_at(k).map(str::to_string);
    SessionReset {
        enabled: f.get("enabled").and_then(Value::bool) == Some(true),
        success: line("successLine"),
        spent: line("spentLine"),
        unavailable: line("unavailableLine"),
    }
}

/// What `line` has in place of `{date}` in `template`, `None` when the
/// line is not that template; `Some("")` for a template with no date.
fn fill<'a>(line: &'a str, template: &str) -> Option<&'a str> {
    match template.split_once("{date}") {
        Some((head, tail)) => {
            let (head, tail) = (head.trim(), tail.trim());
            let start = line.find(head)? + head.len();
            let rest = &line[start..];
            let end = if tail.is_empty() { rest.len() } else { rest.find(tail)? };
            Some(rest[..end].trim())
        }
        None => line.contains(template.trim()).then_some(""),
    }
}

/// The screen after the last echo of the command, or nothing when the
/// screen shows no echo: words above it are not the command's answer.
fn after_command(screen: &str) -> &str {
    let mut from = None;
    let mut pos = 0;
    for line in screen.split_inclusive('\n') {
        pos += line.len();
        let bare = line.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-'));
        if bare == COMMAND {
            from = Some(pos);
        }
    }
    from.map_or("", |i| &screen[i..])
}

/// How the claim went, from the lines the CLI printed after the echo of
/// the command, and the line that says so; `None` while no line says.
pub(super) fn result_of(screen: &str, s: &SessionReset) -> Option<(Res, String)> {
    for line in after_command(screen).lines().map(str::trim).filter(|l| !l.is_empty()) {
        let lower = line.to_ascii_lowercase();
        let by = |t: &Option<String>| t.as_deref().is_some_and(|t| fill(line, t).is_some());
        let result = if by(&s.success) || lower.contains("limits reset") || (lower.contains("reset used ·") && lower.contains("back in your weekly")) {
            Res::Reset
        } else if by(&s.spent) || lower.contains("already used") {
            Res::AlreadyUsed
        } else if line.contains(NOT_AVAILABLE) || by(&s.unavailable) {
            Res::Unavailable
        } else if lower.contains("cooldown") || lower.contains("cooling down") {
            Res::Cooldown
        } else if lower.contains("not eligible") || lower.contains("ineligible") {
            Res::Ineligible
        } else if lower.contains("unknown command") || lower.contains("unknown slash command") {
            Res::Unavailable
        } else {
            continue;
        };
        return Some((result, line.to_string()));
    }
    None
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// The day `text` names, as 00:00 UTC of it: `2026-12-04`, or `Dec 4`
/// (`December 4`, `Dec 4, 2026`) taken as the first such day from a day
/// before `at` on.  The CLI prints a day and no time.
fn day_in(text: &str, at: i64) -> Option<i64> {
    let iso = |y: i64, m: usize, d: u32| marspot::cc_usage::parse_iso_utc(format!("{y:04}-{m:02}-{d:02}T00:00:00Z"));
    if let Some(t) = text.get(..10).and_then(|d| marspot::cc_usage::parse_iso_utc(format!("{d}T00:00:00Z"))) {
        return Some(t);
    }
    let mut words = text.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty());
    let month = words.next()?.to_ascii_lowercase();
    let m = MONTHS.iter().position(|n| month.starts_with(n))? + 1;
    let d: u32 = words.next()?.parse().ok()?;
    if let Some(y) = words.next().and_then(|w| w.parse::<i64>().ok()) {
        return iso(y, m, d);
    }
    let year = 1970 + at / 31_556_952;
    (year - 1..=year + 1).filter_map(|y| iso(y, m, d)).find(|t| *t >= at - 86_400)
}

/// When the session reset is offered again, from the line a claim
/// printed at `at`: the date in the success or spent line, `None` when
/// the line names none this build can read.
pub(super) fn next_offered(line: &str, s: &SessionReset, at: i64) -> Option<i64> {
    [&s.success, &s.spent].into_iter().flatten().filter_map(|t| fill(line, t)).filter(|d| !d.is_empty()).find_map(|d| day_in(d, at))
}
