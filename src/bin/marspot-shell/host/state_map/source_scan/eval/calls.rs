//! Calls: which functions a call reaches, and what they return for
//! the path values passed to them.

use super::super::lex::{is_ident, top_level};
use super::{Args, Env, MAX_DEPTH, Scan, Val, module_of, norm, skip_ws};

/// Whether `text` has a `use` of `name` from the module `m`: `m::name`,
/// `m::{.., name, ..}` (or `name as ..`), or `m::*`.
fn imports(text: &str, m: &str, name: &str) -> bool {
    let b = text.as_bytes();
    let head = format!("{m}::");
    let mut from = 0;
    while let Some(at) = text[from..].find(&head).map(|i| from + i) {
        from = at + 1;
        if at > 0 && is_ident(b[at - 1]) {
            continue;
        }
        let tail = &text[at + head.len()..];
        let named = |t: &str| t.strip_prefix(name).is_some_and(|r| !r.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'));
        if tail.starts_with('*') || named(tail) {
            return true;
        }
        if let Some(group) = tail.strip_prefix('{') {
            let group = &group[..group.find('}').unwrap_or(group.len())];
            if group.split(',').any(|p| named(p.trim())) {
                return true;
            }
        }
    }
    false
}

/// The files the parent module of the module in `file` may be in.
fn parent_files(file: &str) -> [String; 4] {
    let (dir, name) = file.rsplit_once('/').unwrap_or(("", file));
    let module = if name == "mod.rs" { dir.to_string() } else { format!("{dir}/{}", name.trim_end_matches(".rs")) };
    let parent = module.rsplit_once('/').map_or("", |(p, _)| p);
    [format!("{parent}.rs"), format!("{parent}/mod.rs"), format!("{parent}/main.rs"), format!("{parent}/lib.rs")]
}

impl Scan<'_> {
    /// The functions a call of the path `q` may reach.
    pub(super) fn resolve(&self, si: usize, q: &[&str]) -> Vec<(usize, usize)> {
        let last = q[q.len() - 1];
        let Some(all) = self.by_name.get(last) else { return Vec::new() };
        let qual = (q.len() >= 2).then(|| q[q.len() - 2]).filter(|w| !["self", "Self", "super", "crate"].contains(w));
        let fn_of = |&(s, f): &(usize, usize)| &self.srcs[s].fns[f];
        match qual {
            Some(w) => {
                let w = norm(w);
                all.iter().copied().filter(|c| fn_of(c).owner.as_deref().is_some_and(|o| norm(o) == w) || norm(module_of(&self.srcs[c.0].file)) == w).collect()
            }
            None => {
                let here: Vec<_> = all.iter().copied().filter(|c| c.0 == si).collect();
                if !here.is_empty() {
                    return here;
                }
                let free: Vec<_> = all.iter().copied().filter(|c| fn_of(c).owner.is_none()).collect();
                if all.len() == 1 {
                    return free;
                }
                // a name defined in several files: the ones this file brings into scope
                let caller = &self.srcs[si];
                let parents = parent_files(&caller.file);
                free.into_iter()
                    .filter(|c| {
                        let callee = self.srcs[c.0].file.as_str();
                        imports(&caller.text, module_of(callee), last) || (parents.iter().any(|p| p == callee) && imports(&caller.text, "super", last))
                    })
                    .collect()
            }
        }
    }

    pub(super) fn args(&mut self, si: usize, range: std::ops::Range<usize>, env: &mut Env) -> Args {
        let srcs = self.srcs;
        let b = srcs[si].text.as_bytes();
        let mut out = Vec::new();
        let mut at = range.start;
        while at < range.end {
            let end = top_level(&b[..range.end], at, b",");
            let v = self.eval(si, at, env).filter(|(_, e)| skip_ws(b, *e) >= end).map(|(v, _)| v);
            out.push(v);
            at = end + 1;
        }
        out
    }

    /// What function `f` returns for `args`, walking its body with its
    /// parameters bound to them.
    pub(super) fn call(&mut self, f: (usize, usize), args: Args) -> Vec<Val> {
        let key = (f.0, f.1, args);
        if let Some(v) = self.memo.get(&key) {
            return v.clone();
        }
        if self.busy.contains(&f) || self.busy.len() >= MAX_DEPTH {
            return Vec::new();
        }
        self.busy.insert(f);
        let def = &self.srcs[f.0].fns[f.1];
        let (body, braced) = (def.body.clone(), def.braced);
        let mut env: Env = def.params.iter().cloned().zip(key.2.iter().cloned()).filter_map(|(p, v)| Some((p, v?))).collect();
        self.walk(f.0, body.clone(), &mut env);
        let ret = self.tail(f.0, body, braced, &mut env);
        self.busy.remove(&f);
        self.memo.insert(key, ret.clone());
        ret
    }

    /// The path value of the last expression of a body.
    pub(super) fn tail(&mut self, si: usize, body: std::ops::Range<usize>, braced: bool, env: &mut Env) -> Vec<Val> {
        let srcs = self.srcs;
        let b = srcs[si].text.as_bytes();
        let mut s = body.start;
        if braced {
            loop {
                let semi = top_level(&b[..body.end], s, b";");
                if semi >= body.end {
                    break;
                }
                s = semi + 1;
            }
        }
        let text = &srcs[si].text;
        let mut e = body.start + text[body.start..body.end].trim_end().len();
        s = skip_ws(b, s);
        if s >= e {
            return Vec::new();
        }
        for wrap in ["Ok(", "Some("] {
            if text[s..e].starts_with(wrap) && text[..e].ends_with(')') {
                s += wrap.len();
                e -= 1;
            }
        }
        match self.eval(si, s, env) {
            Some((vals, end)) if skip_ws(b, end) >= e => vals,
            _ => Vec::new(),
        }
    }
}
