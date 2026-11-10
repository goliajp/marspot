//! The model and the thinking setting a kimi starts with, worked out
//! from `<home>/config.toml` in the order kimi itself resolves them.
//!
//! This is what a pane shows before its first prompt, when there is no
//! session to read.  kimi takes `default_model`, looks the alias up in
//! `[models."<alias>"]`, and settles the thinking effort from
//! `[thinking]` and that model's own efforts.  The file is the user's
//! and is read by line: the handful of keys needed are single-line
//! strings, booleans and string arrays as kimi writes them.  Where the
//! file does not say enough to be certain, the answer is "not known"
//! and the badge leaves that part out.

use std::path::Path;

/// What a kimi with no session would use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Defaults {
    /// `default_model`, as written.
    pub alias: Option<String>,
    /// `off`, `on`, or one of the model's efforts.
    pub thinking: Option<String>,
}

pub(super) fn defaults(home: &Path) -> Defaults {
    std::fs::read_to_string(home.join("config.toml")).map(|t| defaults_in(&t)).unwrap_or_default()
}

/// The lines of the table whose header is exactly `header`; the
/// top-level keys for an empty header.
fn table<'a>(text: &'a str, header: &'a str) -> impl Iterator<Item = &'a str> {
    let mut inside = header.is_empty();
    text.lines().map(str::trim).filter(move |line| {
        if line.starts_with('[') {
            inside = *line == header;
            return false;
        }
        inside
    })
}

/// The value of `key` in a table's lines, as written.
fn value<'a>(mut lines: impl Iterator<Item = &'a str>, key: &str) -> Option<&'a str> {
    lines.find_map(|l| Some(l.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim()))
}

fn string(v: &str) -> Option<&str> {
    let inner = v.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.is_empty() && !inner.contains(['"', '\\'])).then_some(inner)
}

/// A single-line array of plain strings; None for anything else.
fn strings(v: &str) -> Option<Vec<&str>> {
    let inner = v.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.trim_end_matches(',').split(',').map(|s| string(s.trim())).collect()
}

/// An alias or an effort short and plain enough for a badge and a
/// command line.
pub(super) fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/:".contains(&b))
}

pub(super) fn defaults_in(text: &str) -> Defaults {
    let Some(alias) = value(table(text, ""), "default_model").and_then(string).filter(|a| plain(a)) else {
        return Defaults::default();
    };
    Defaults { thinking: thinking_for(text, alias), alias: Some(alias.to_string()) }
}

/// The effort kimi settles on for `alias` with nothing requested.
fn thinking_for(text: &str, alias: &str) -> Option<String> {
    let header = format!("[models.\"{alias}\"]");
    let model = || table(text, &header);
    // a model that is not in the file is one this cannot speak for
    model().next()?;
    let list = |key: &str| match value(model(), key) {
        Some(v) => strings(v),
        None => Some(Vec::new()),
    };
    let (capabilities, efforts) = (list("capabilities")?, list("support_efforts")?);
    let always = capabilities.contains(&"always_thinking");
    let thinks = always || capabilities.contains(&"thinking");
    let own = if !thinks {
        "off"
    } else if efforts.is_empty() {
        "on"
    } else {
        let declared = value(model(), "default_effort").and_then(string);
        declared.filter(|d| efforts.contains(d)).unwrap_or(efforts[efforts.len() / 2])
    };
    let configured = value(table(text, "[thinking]"), "effort").and_then(string).map(str::to_ascii_lowercase);
    let configured = configured.as_deref();
    let disabled = value(table(text, "[thinking]"), "enabled") == Some("false");
    let mut effort = if disabled { "off" } else { configured.unwrap_or(own) };
    if effort == "off" && always {
        effort = configured.filter(|c| *c != "off").unwrap_or(own);
    }
    let settled = match effort {
        "off" => "off",
        "on" if !efforts.is_empty() => own,
        e if efforts.contains(&e) || (e == "on" && thinks) => e,
        // an effort the model does not declare: kimi keeps it or
        // replaces it depending on how the session was started
        _ => return None,
    };
    plain(settled).then(|| settled.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables a 2.5.3 login writes, with one model that declares
    /// efforts and one that does not.
    const LOGGED_IN: &str = r#"default_model = "kimi-code/kimi-for-coding"

[providers."managed:kimi-code"]
type = "kimi"

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
capabilities = [ "thinking", "always_thinking", "image_in", "tool_use" ]
display_name = "K2.8 Preview"
support_efforts = [ "low", "high", "max" ]
default_effort = "max"

[models."kimi-code/kimi-for-coding-highspeed"]
provider = "managed:kimi-code"
model = "kimi-for-coding-highspeed"
capabilities = [ "thinking", "always_thinking", "image_in", "tool_use" ]
display_name = "K2.7 Code Highspeed"

[models."kimi-code/k3"]
provider = "managed:kimi-code"
model = "k3"
capabilities = [ "thinking", "always_thinking", "tool_use" ]
support_efforts = [ "low", "high", "max" ]
default_effort = "high"

[models."local/plain"]
model = "plain"
capabilities = [ "tool_use" ]

[thinking]
enabled = true
effort = "max"
"#;

    fn with(text: &str, from: &str, to: &str) -> Defaults {
        assert!(text.contains(from), "{from}");
        defaults_in(&text.replace(from, to))
    }

    fn got(d: &Defaults) -> (Option<&str>, Option<&str>) {
        (d.alias.as_deref(), d.thinking.as_deref())
    }

    #[test]
    fn the_default_model_and_the_configured_effort() {
        assert_eq!(got(&defaults_in(LOGGED_IN)), (Some("kimi-code/kimi-for-coding"), Some("max")));
        let k3 = with(LOGGED_IN, "default_model = \"kimi-code/kimi-for-coding\"", "default_model = \"kimi-code/k3\"");
        assert_eq!(got(&k3), (Some("kimi-code/k3"), Some("max")), "the configured effort wins over the model's own");
        let unset = with(LOGGED_IN, "\neffort = \"max\"\n", "\n");
        assert_eq!(got(&unset).1, Some("max"), "the model's declared default");
        let k3_unset = with(&LOGGED_IN.replace("\neffort = \"max\"\n", "\n"), "default_model = \"kimi-code/kimi-for-coding\"", "default_model = \"kimi-code/k3\"");
        assert_eq!(got(&k3_unset).1, Some("high"));
    }

    #[test]
    fn a_model_that_always_thinks_is_never_off() {
        let disabled = with(LOGGED_IN, "enabled = true", "enabled = false");
        assert_eq!(got(&disabled).1, Some("max"), "the configured effort stands in for off");
        let off = with(&LOGGED_IN.replace("enabled = true", "enabled = false"), "\neffort = \"max\"\n", "\n");
        assert_eq!(got(&off).1, Some("max"), "and without one, the model's own default");
    }

    #[test]
    fn a_model_without_efforts_is_on_or_off() {
        let fast = "default_model = \"kimi-code/kimi-for-coding-highspeed\"";
        let d = with(&LOGGED_IN.replace("\neffort = \"max\"\n", "\n"), "default_model = \"kimi-code/kimi-for-coding\"", fast);
        assert_eq!(got(&d).1, Some("on"));
        let plain = with(&LOGGED_IN.replace("\neffort = \"max\"\n", "\n"), "default_model = \"kimi-code/kimi-for-coding\"", "default_model = \"local/plain\"");
        assert_eq!(got(&plain).1, Some("off"), "a model that cannot think");
    }

    #[test]
    fn what_the_file_does_not_settle_is_not_known() {
        let fast = with(LOGGED_IN, "default_model = \"kimi-code/kimi-for-coding\"", "default_model = \"kimi-code/kimi-for-coding-highspeed\"");
        assert_eq!(got(&fast), (Some("kimi-code/kimi-for-coding-highspeed"), None), "an effort the model does not declare");
        let gone = with(LOGGED_IN, "default_model = \"kimi-code/kimi-for-coding\"", "default_model = \"other/model\"");
        assert_eq!(got(&gone), (Some("other/model"), None), "a model the file does not describe");
        let multiline = with(LOGGED_IN, "support_efforts = [ \"low\", \"high\", \"max\" ]\ndefault_effort = \"max\"", "support_efforts = [\n  \"low\",\n]");
        assert_eq!(got(&multiline).1, None, "an array this does not read");
        assert_eq!(defaults_in("[models.x]\ndefault_model = \"inner\"\n"), Defaults::default());
        assert_eq!(defaults_in(""), Defaults::default());
    }
}
