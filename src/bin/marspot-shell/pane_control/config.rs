//! The numbers and switches the control socket follows: `control_*`
//! keys in `settings.toml` (re-read whenever L1 sees the file change),
//! and the directories whose panes may not use it, one per line in
//! `<state dir>/control-deny.tsv`.

use std::collections::HashSet;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Config {
    pub enabled: bool,
    pub shadows_total: usize,
    pub shadows_per_parent: usize,
    pub shadow_depth: usize,
    pub window_panes_per_caller: usize,
    pub user_headroom: usize,
    pub watches_per_caller: usize,
    pub watches_total: usize,
    pub connections: usize,
    pub rate_input: u32,
    pub rate_read: u32,
    pub rate_global: u32,
    pub spawn_per_minute: u32,
    pub pane_bytes_per_s: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            shadows_total: 24,
            shadows_per_parent: 8,
            shadow_depth: 2,
            window_panes_per_caller: 8,
            user_headroom: 8,
            watches_per_caller: 8,
            watches_total: 64,
            connections: 64,
            rate_input: 20,
            rate_read: 20,
            rate_global: 200,
            spawn_per_minute: 6,
            pane_bytes_per_s: 128 * 1024,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        Config::parse(&std::fs::read_to_string(marspot::settings::path()).unwrap_or_default())
    }

    /// The `control_*` keys of a settings file; anything missing or
    /// unreadable keeps its default.
    pub fn parse(body: &str) -> Config {
        let mut c = Config::default();
        for line in body.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let Some(k) = k.trim().strip_prefix("control_") else { continue };
            let v = v.split('#').next().unwrap_or("").trim().trim_matches('"');
            let n = v.parse::<u32>().ok().filter(|n| *n > 0);
            let set = |slot: &mut usize| {
                if let Some(n) = n {
                    *slot = n as usize;
                }
            };
            let set32 = |slot: &mut u32| {
                if let Some(n) = n {
                    *slot = n;
                }
            };
            match k {
                "enabled" => c.enabled = !matches!(v, "false" | "off" | "0" | "no"),
                "shadows_total" => set(&mut c.shadows_total),
                "shadows_per_parent" => set(&mut c.shadows_per_parent),
                "shadow_depth" => set(&mut c.shadow_depth),
                "window_panes_per_caller" => set(&mut c.window_panes_per_caller),
                "user_headroom" => set(&mut c.user_headroom),
                "watches_per_caller" => set(&mut c.watches_per_caller),
                "watches_total" => set(&mut c.watches_total),
                "connections" => set(&mut c.connections),
                "rate_input" => set32(&mut c.rate_input),
                "rate_read" => set32(&mut c.rate_read),
                "rate_global" => set32(&mut c.rate_global),
                "spawn_per_minute" => set32(&mut c.spawn_per_minute),
                "pane_bytes_per_s" => set32(&mut c.pane_bytes_per_s),
                _ => {}
            }
        }
        c
    }
}

/// The deny list, read again when its file's modification time moves.
/// A directory listed denies the panes in it and below it.
pub(super) struct DenyList {
    path: std::path::PathBuf,
    stamp: Option<SystemTime>,
    dirs: HashSet<String>,
}

pub(super) fn deny_path() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("control-deny.tsv")
}

fn norm(dir: &str) -> &str {
    let t = dir.trim();
    if t.len() > 1 { t.trim_end_matches('/') } else { t }
}

impl DenyList {
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path, stamp: None, dirs: HashSet::new() }
    }

    /// Whether panes in `cwd` are refused, after a `stat` of the file.
    pub fn denies(&mut self, cwd: &str) -> bool {
        let stamp = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if stamp != self.stamp {
            self.stamp = stamp;
            self.dirs = std::fs::read_to_string(&self.path)
                .unwrap_or_default()
                .lines()
                .map(norm)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect();
        }
        let cwd = norm(cwd);
        self.dirs.iter().any(|d| within(cwd, d))
    }
}

/// `path` is `dir` or below it, component by component.
pub(super) fn within(path: &str, dir: &str) -> bool {
    match path.strip_prefix(dir) {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || dir.ends_with('/'),
        None => false,
    }
}
