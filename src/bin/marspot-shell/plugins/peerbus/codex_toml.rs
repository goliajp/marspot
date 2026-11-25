//! The bits of TOML text the Codex config edits read and write.

/// The delimiter of the multi-line string `value` opens and does not
/// close.
pub(super) fn opened_by(value: &str) -> Option<&'static str> {
    ["\"\"\"", "'''"].into_iter().find(|d| closes(value, d))
}

/// Whether `line` ends on the other side of a `delim` string than it
/// starts on.
pub(super) fn closes(line: &str, delim: &str) -> bool {
    line.matches(delim).count() % 2 == 1
}

/// The table a header line names, without spaces and key quotes:
/// `[ mcp_servers."marspot" ] # x` is `mcp_servers.marspot`.
pub(super) fn header(body: &str) -> String {
    let mut name = String::new();
    let mut quote: Option<char> = None;
    for c in body.trim_start_matches('[').chars() {
        match (quote, c) {
            (None, ']') => break,
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ' ' | '\t') => {}
            _ => name.push(c),
        }
    }
    name
}

/// The length of the one-line string `value` starts with, when the
/// string is all the line holds besides a comment.
pub(super) fn string_token(value: &str) -> Option<usize> {
    let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return None;
    }
    let mut escaped = false;
    let end = value[1..].char_indices().find_map(|(i, c)| {
        let closes = c == quote && !escaped;
        escaped = quote == '"' && c == '\\' && !escaped;
        closes.then_some(i + 2)
    })?;
    let rest = value[end..].trim();
    (rest.is_empty() || rest.starts_with('#')).then_some(end)
}

/// What a one-line string token says.  None for an escape other than
/// the two `toml_string` writes: such a value is not one written here,
/// and it is repointed.
pub(super) fn string_value(token: &str) -> Option<String> {
    let inner = token.get(1..token.len().checked_sub(1)?)?;
    if token.starts_with('\'') {
        return Some(inner.to_string());
    }
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().filter(|e| matches!(e, '\\' | '"'))?),
            c => out.push(c),
        }
    }
    Some(out)
}

pub(super) fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
