//! Following path values through the source: from a base, through
//! `let`s, method chains, and calls to functions and closures that take
//! or return a path.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::lex::{FnDef, close_of, is_ident, literal_end, top_level};

mod calls;
mod chain;
pub(super) use chain::with_extension;

/// Stands in a path for a part the source computes: a `format!`
/// argument, a number turned into text.
pub(super) const HOLE: char = '\u{1}';

/// The paths the source starts from, by the name of the function that
/// returns them, and where under the state root they are.
const BASES: &[(&str, &str)] = &[("state_root", ""), ("sessions_dir", "sessions"), ("binaries_root", "binaries"), ("log_dir", "logs")];

/// Calls deep enough to follow a path through a helper of a helper.
const MAX_DEPTH: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Val {
    pub path: String,
    /// An `OsString`: `push` appends text instead of adding a segment.
    pub os: bool,
}

pub(super) struct Src {
    pub file: String,
    pub text: String,
    pub fns: Vec<FnDef>,
}

type Env = HashMap<String, Vec<Val>>;
type Args = Vec<Option<Vec<Val>>>;

pub(super) struct Scan<'a> {
    srcs: &'a [Src],
    consts: &'a HashMap<String, Vec<(String, String)>>,
    by_name: HashMap<&'a str, Vec<(usize, usize)>>,
    memo: HashMap<(usize, usize, Args), Vec<Val>>,
    busy: HashSet<(usize, usize)>,
    /// Struct fields set to a path, by source and field name, read back
    /// as `self.<field>` in that source.
    fields: HashMap<(usize, String), Vec<Val>>,
    /// (path, file it is built in)
    pub found: BTreeSet<(String, String)>,
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn ident_len(b: &[u8], at: usize) -> usize {
    b[at..].iter().take_while(|&&c| is_ident(c)).count()
}

fn norm(s: &str) -> String {
    s.chars().filter(|&c| c != '_').flat_map(char::to_lowercase).collect()
}

fn module_of(file: &str) -> &str {
    let mut parts = file.rsplit('/');
    let stem = parts.next().unwrap_or(file).trim_end_matches(".rs");
    if stem == "mod" { parts.next().unwrap_or(stem) } else { stem }
}


impl<'a> Scan<'a> {
    pub(super) fn new(srcs: &'a [Src], consts: &'a HashMap<String, Vec<(String, String)>>) -> Scan<'a> {
        let mut by_name: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
        for (si, s) in srcs.iter().enumerate() {
            for (fi, f) in s.fns.iter().enumerate() {
                by_name.entry(f.name.as_str()).or_default().push((si, fi));
            }
        }
        Scan { srcs, consts, by_name, memo: HashMap::new(), busy: HashSet::new(), fields: HashMap::new(), found: BTreeSet::new() }
    }

    /// Start another pass over the sources: what a call returns may now
    /// depend on fields learnt in the pass before.
    pub(super) fn next_pass(&mut self) {
        self.memo.clear();
    }

    /// Walk `range` of source `si` in order, binding `let`s in `env`.
    pub(super) fn walk(&mut self, si: usize, range: std::ops::Range<usize>, env: &mut Env) {
        let srcs = self.srcs;
        let b = srcs[si].text.as_bytes();
        let mut i = range.start;
        let mut after_fn = false;
        while i < range.end {
            if let Some(end) = literal_end(b, i) {
                i = end;
                continue;
            }
            if !is_ident(b[i]) || (i > 0 && is_ident(b[i - 1])) || b[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let len = ident_len(b, i);
            let word = &srcs[si].text[i..i + len];
            let method = srcs[si].text[..i].trim_end().ends_with('.');
            match word {
                "fn" => env.clear(),
                "let" => self.bind_let(si, i + len, env),
                _ if !method && !after_fn => {
                    self.field_init(si, i, len, env);
                    self.eval(si, i, env);
                }
                _ => {}
            }
            after_fn = word == "fn";
            i += len;
        }
    }

    /// `Struct { field, .. }` or `{ field: <path>, .. }` at `at`: the
    /// field holds that path.
    fn field_init(&mut self, si: usize, at: usize, len: usize, env: &mut Env) {
        let srcs = self.srcs;
        let (text, b) = (srcs[si].text.as_str(), srcs[si].text.as_bytes());
        if !text[..at].trim_end().ends_with(['{', ',']) {
            return;
        }
        let name = &text[at..at + len];
        let next = skip_ws(b, at + len);
        let vals = match b.get(next) {
            Some(b',' | b'}') => env.get(name).cloned(),
            Some(b':') if b.get(next + 1) != Some(&b':') => self.eval(si, next + 1, env).map(|(v, _)| v),
            _ => None,
        };
        if let Some(v) = vals {
            let have = self.fields.entry((si, name.to_string())).or_default();
            have.extend(v);
            have.sort();
            have.dedup();
        }
    }

    fn bind_let(&mut self, si: usize, at: usize, env: &mut Env) {
        let srcs = self.srcs;
        let text = &srcs[si].text;
        let b = text.as_bytes();
        let mut p = skip_ws(b, at);
        for wrap in ["mut ", "Ok(", "Some("] {
            if text[p..].starts_with(wrap) {
                p = skip_ws(b, p + wrap.len());
            }
        }
        let var = text[p..p + ident_len(b, p)].to_string();
        let eq = top_level(b, p, b"=;");
        if var.is_empty() || b.get(eq) != Some(&b'=') || b.get(eq + 1) == Some(&b'=') {
            return;
        }
        let bound = self.eval(si, eq + 1, env).filter(|(_, end)| {
            let mut next = skip_ws(b, *end);
            if b.get(next) == Some(&b'?') {
                next = skip_ws(b, next + 1);
            }
            b.get(next) == Some(&b';') || text[next..].starts_with("else")
        });
        match bound {
            Some((vals, _)) => env.insert(var, vals),
            None => env.remove(&var),
        };
    }

    /// The path values of the expression at `at` and where it ends.
    pub(super) fn eval(&mut self, si: usize, at: usize, env: &mut Env) -> Option<(Vec<Val>, usize)> {
        let (vals, end, var) = self.start(si, at, env)?;
        Some(self.chain(si, end, vals, var, env))
    }

    fn start(&mut self, si: usize, at: usize, env: &mut Env) -> Option<(Vec<Val>, usize, Option<String>)> {
        let srcs = self.srcs;
        let src = &srcs[si];
        let (text, b) = (src.text.as_str(), src.text.as_bytes());
        let mut p = skip_ws(b, at);
        while matches!(b.get(p), Some(b'&' | b'*')) || text[p..].starts_with("mut ") {
            p = skip_ws(b, p + if b[p] == b'm' { 4 } else { 1 });
        }
        let mut q: Vec<&str> = Vec::new();
        let mut j = p;
        loop {
            let len = ident_len(b, j);
            if len == 0 {
                break;
            }
            q.push(&text[j..j + len]);
            j += len;
            if !text[j..].starts_with("::") || text[j + 2..].starts_with('<') {
                break;
            }
            j += 2;
        }
        let last = *q.last()?;
        let open = skip_ws(b, j);
        if b.get(open) == Some(&b'(') {
            let close = close_of(b, open);
            let args = text[open + 1..close - 1].trim();
            let tail2 = |a: &str, z: &str| q.len() >= 2 && q[q.len() - 2] == a && last == z;
            if let Some((_, prefix)) = BASES.iter().find(|(n, _)| *n == last).filter(|_| args.is_empty()) {
                return Some((vec![Val { path: prefix.to_string(), os: false }], close, None));
            }
            if last == "session_dir" && !args.is_empty() && !args.contains(',') {
                return Some((vec![Val { path: format!("sessions/{HOLE}"), os: false }], close, None));
            }
            if tail2("StateStore", "open") {
                let name = args.strip_prefix('"')?.strip_suffix('"')?;
                return Some((vec![Val { path: name.to_string(), os: false }], close, None));
            }
            if tail2("PathBuf", "from") || tail2("Path", "new") || tail2("OsString", "from") {
                let os = q[q.len() - 2] == "OsString";
                let (vals, end) = self.eval(si, open + 1, env)?;
                let vals = vals.into_iter().map(|v| Val { os, ..v }).collect();
                return (skip_ws(b, end) == close - 1).then_some((vals, close, None));
            }
            let fns = self.resolve(si, &q);
            if fns.is_empty() {
                return None;
            }
            let args = self.args(si, open + 1..close - 1, env);
            let mut out: Vec<Val> = Vec::new();
            for f in fns {
                out.extend(self.call(f, args.clone()));
            }
            out.sort();
            out.dedup();
            return (!out.is_empty()).then_some((out, close, None));
        }
        if q.len() != 1 {
            return None;
        }
        if last == "self" {
            let f = skip_ws(b, j);
            if b.get(f) != Some(&b'.') {
                return None;
            }
            let name = &text[f + 1..f + 1 + ident_len(b, f + 1)];
            let vals = self.fields.get(&(si, name.to_string()))?.clone();
            return Some((vals, f + 1 + name.len(), None));
        }
        let plugin = src.file.split("/plugins/").nth(1).and_then(|r| r.split_once('/')).map(|(p, _)| p);
        if let Some(p) = plugin.filter(|_| text[j..].trim_start().starts_with(".state_dir()")) {
            let end = j + text[j..].find(".state_dir()").expect("just seen") + ".state_dir()".len();
            return Some((vec![Val { path: format!("plugins/{p}"), os: false }], end, None));
        }
        env.get(last).map(|v| (v.clone(), j, Some(last.to_string())))
    }
}
