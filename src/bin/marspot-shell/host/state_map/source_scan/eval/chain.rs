//! The method chains a path value goes through, and the text a
//! `.join(..)` or `.push(..)` adds.

use super::super::lex::{close_of, is_ident, literal_end};
use super::{Env, HOLE, Scan, Val, ident_len, module_of, norm, skip_ws};

/// The text of a `format!` template with each argument a [`HOLE`].
fn template(t: &str) -> String {
    let mut out = String::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => out.extend(it.next()),
            '}' if it.peek() == Some(&'}') => out.extend(it.next()),
            '{' => {
                it.by_ref().find(|&c| c == '}');
                out.push(HOLE);
            }
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn with_extension(path: &str, ext: &str) -> String {
    let seg_at = path.rfind('/').map_or(0, |i| i + 1);
    let stem_end = path[seg_at..].rfind('.').filter(|&d| d > 0).map_or(path.len(), |d| seg_at + d);
    let stem = &path[..stem_end];
    if ext.is_empty() { stem.to_string() } else { format!("{stem}.{ext}") }
}

fn joined(path: &str, part: &str) -> String {
    if path.is_empty() { part.to_string() } else { format!("{path}/{part}") }
}

impl Scan<'_> {
    /// The text values of one argument that names part of a path.
    pub(super) fn part(&self, si: usize, arg: &str) -> Option<Vec<String>> {
        let a = arg.trim();
        let a = a.strip_prefix('&').unwrap_or(a).trim();
        if let Some(lit) = a.strip_prefix('"') {
            let end = literal_end(a.as_bytes(), 0)?;
            return (end == a.len()).then(|| vec![lit[..end - 2].to_string()]);
        }
        if let Some(inner) = a.strip_prefix("format!(").and_then(|r| r.strip_suffix(')')) {
            let inner = inner.trim_start();
            let end = literal_end(inner.as_bytes(), 0).filter(|_| inner.starts_with('"'))?;
            return Some(vec![template(&inner[1..end - 1])]);
        }
        if let Some(var) = a.strip_suffix(".to_string()").filter(|v| v.bytes().all(is_ident)) {
            return (!var.is_empty()).then(|| vec![HOLE.to_string()]);
        }
        let name = a.rsplit("::").next()?;
        if name.is_empty() || !a.bytes().all(|c| is_ident(c) || c == b':') || !name.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_') {
            return None;
        }
        let all = self.consts.get(name)?;
        let file = &self.srcs[si].file;
        let qual = a.rsplit("::").nth(1).filter(|w| !["self", "Self", "super", "crate"].contains(w)).map(norm);
        let here: Vec<String> = match qual {
            // `module::NAME`: the constant of that module
            Some(w) => all.iter().filter(|(f, _)| norm(module_of(f)) == w).map(|(_, v)| v.clone()).collect(),
            None => all.iter().filter(|(f, _)| f == file).map(|(_, v)| v.clone()).collect(),
        };
        let mut vals = if here.is_empty() { all.iter().map(|(_, v)| v.clone()).collect() } else { here };
        vals.sort();
        vals.dedup();
        Some(vals)
    }

    /// Apply the method chain at `at` to `vals`; a `push` on a bound
    /// variable updates it.
    pub(super) fn chain(&mut self, si: usize, mut at: usize, mut vals: Vec<Val>, var: Option<String>, env: &mut Env) -> (Vec<Val>, usize) {
        let srcs = self.srcs;
        let text = &srcs[si].text;
        let b = text.as_bytes();
        let mut grew = false;
        loop {
            let mut p = skip_ws(b, at);
            if b.get(p) == Some(&b'?') {
                p = skip_ws(b, p + 1);
            }
            if b.get(p) != Some(&b'.') {
                break;
            }
            let m = skip_ws(b, p + 1);
            let name = &text[m..m + ident_len(b, m)];
            let open = skip_ws(b, m + name.len());
            if b.get(open) != Some(&b'(') {
                break;
            }
            let close = close_of(b, open);
            let arg = &text[open + 1..close - 1];
            match name {
                "join" | "with_extension" | "with_file_name" | "push" => {
                    // a part the scan cannot read is a hole: the path must still match a row
                    let parts = self.part(si, arg).unwrap_or_else(|| vec![HOLE.to_string()]);
                    vals = vals
                        .iter()
                        .flat_map(|v| {
                            parts.iter().map(move |part| match name {
                                "with_extension" => Val { path: with_extension(&v.path, part), ..v.clone() },
                                "with_file_name" => Val { path: joined(&v.path[..v.path.rfind('/').unwrap_or(0)], part), ..v.clone() },
                                "push" if v.os => Val { path: format!("{}{part}", v.path), ..v.clone() },
                                _ => Val { path: joined(&v.path, part), ..v.clone() },
                            })
                        })
                        .collect();
                    grew = true;
                    if name == "push" {
                        if let Some(var) = &var {
                            env.insert(var.clone(), vals.clone());
                        }
                    }
                }
                "as_os_str" | "into_os_string" | "to_os_string" => vals.iter_mut().for_each(|v| v.os = true),
                "to_path_buf" | "to_owned" | "clone" | "as_path" | "as_ref" | "into" | "unwrap" if arg.trim().is_empty() => {}
                "expect" => {}
                _ => break,
            }
            at = close;
        }
        if grew {
            let file = &srcs[si].file;
            // an `OsString` that ends in a dot is a name half assembled (`p.push(".");
            // p.push(ext)`), no file
            let whole = vals.iter().filter(|v| !(v.os && v.path.ends_with('.')));
            self.found.extend(whole.map(|v| (v.path.clone(), file.clone())));
        }
        (vals, at)
    }
}
