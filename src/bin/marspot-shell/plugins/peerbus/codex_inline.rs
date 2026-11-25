//! The one-line inline table, `{ K = "v", … }`, a server's `env` is
//! written as in a Codex `config.toml`: found, and one pair in it set
//! without touching the others.

use std::ops::Range;

use super::codex_toml::{string_value, toml_string};

/// Where quotes and brackets leave a scan of one line of TOML.
#[derive(Default)]
struct Nesting {
    quote: Option<char>,
    escaped: bool,
    depth: usize,
}

impl Nesting {
    /// Steps over `c`; true when `c` is outside every string.
    fn step(&mut self, c: char) -> bool {
        if let Some(q) = self.quote {
            if self.escaped {
                self.escaped = false;
            } else if q == '"' && c == '\\' {
                self.escaped = true;
            } else if c == q {
                self.quote = None;
            }
            return false;
        }
        if matches!(c, '"' | '\'') {
            self.quote = Some(c);
            return false;
        }
        true
    }
}

/// The length of the inline table `value` starts with, its closing
/// brace included; None when it does not close on the line.
pub(super) fn table_len(value: &str) -> Option<usize> {
    let mut n = Nesting::default();
    for (i, c) in value.char_indices() {
        if !n.step(c) {
            continue;
        }
        match c {
            '{' | '[' => n.depth += 1,
            '}' | ']' => {
                n.depth = n.depth.checked_sub(1)?;
                if n.depth == 0 {
                    return (c == '}').then_some(i + 1);
                }
            }
            '#' | '\n' => return None,
            _ => {}
        }
    }
    None
}

/// The pairs of `table` (an inline table, braces included): each key
/// without spaces and quotes, and where its value is.
fn pairs(table: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let mut n = Nesting::default();
    // the start of the current pair, and its `=` once seen
    let (mut from, mut eq) = (1, None);
    for (i, c) in table.char_indices().skip(1) {
        let outside = n.step(c);
        match c {
            '{' | '[' if outside => n.depth += 1,
            '}' | ']' if outside && n.depth > 0 => n.depth -= 1,
            '=' if outside && n.depth == 0 && eq.is_none() => eq = Some(i),
            ',' | '}' if outside && n.depth == 0 => {
                if let Some(e) = eq {
                    let key = table[from..e].chars().filter(|c| !matches!(c, ' ' | '\t' | '"' | '\'')).collect();
                    let raw = &table[e + 1..i];
                    let start = e + 1 + (raw.len() - raw.trim_start().len());
                    out.push((key, start..start + raw.trim().len()));
                }
                (from, eq) = (i + 1, None);
            }
            _ => {}
        }
    }
    out
}

/// `table` with every pair of `env` in it saying its value: a pair
/// present has its value replaced, a missing one is added before the
/// closing brace, every other pair and byte stays.  None when the
/// table already says all of them.
pub(super) fn with_pairs(table: &str, env: &[(&str, String)]) -> Option<String> {
    let have = pairs(table);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (k, v) in env {
        match have.iter().find(|(key, _)| key == k) {
            Some((_, r)) if is_string(&table[r.clone()], v) => {}
            Some((_, r)) => edits.push((r.clone(), toml_string(v))),
            None => missing.push(format!("{k} = {}", toml_string(v))),
        }
    }
    if edits.is_empty() && missing.is_empty() {
        return None;
    }
    let mut out = table.to_string();
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    for (r, v) in edits {
        out.replace_range(r, &v);
    }
    if !missing.is_empty() {
        let head = out[..out.len() - 1].trim_end();
        let sep = if head.ends_with('{') || head.ends_with(',') { " " } else { ", " };
        let tail = if out[head.len()..] == *"}" { " }".to_string() } else { out[head.len()..].to_string() };
        out = format!("{head}{sep}{}{tail}", missing.join(", "));
    }
    Some(out)
}

/// Whether the value token `token` is the string `v`.
fn is_string(token: &str, v: &str) -> bool {
    token.starts_with(['"', '\'']) && string_value(token).as_deref() == Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(v: &str) -> Vec<(&'static str, String)> {
        vec![("MADCAWL_SOCKET", v.to_string())]
    }

    #[test]
    fn codex_inline_table_ends_at_its_closing_brace() {
        assert_eq!(table_len("{ A = \"}\", B = [1, 2] } # c\n"), Some(23));
        assert_eq!(table_len("{ A = \"x\",\n"), None);
        assert_eq!(table_len("{ A = 1 # }\n"), None);
    }

    #[test]
    fn codex_inline_pair_is_replaced_added_or_left() {
        let t = "{ MADCAWL_SOCKET = \"x\", RUST_LOG = \"debug\" }";
        assert_eq!(with_pairs(t, &env("/s")).unwrap(), "{ MADCAWL_SOCKET = \"/s\", RUST_LOG = \"debug\" }");
        assert_eq!(with_pairs(t, &env("x")), None);
        assert_eq!(with_pairs("{ RUST_LOG = \"a,b\" }", &env("/s")).unwrap(), "{ RUST_LOG = \"a,b\", MADCAWL_SOCKET = \"/s\" }");
        assert_eq!(with_pairs("{}", &env("/s")).unwrap(), "{ MADCAWL_SOCKET = \"/s\" }");
        assert_eq!(with_pairs("{ \"MADCAWL_SOCKET\" = 'x' }", &env("x")), None);
        assert_eq!(with_pairs("{ MADCAWL_SOCKET = 5 }", &env("5")).unwrap(), "{ MADCAWL_SOCKET = \"5\" }");
    }
}
