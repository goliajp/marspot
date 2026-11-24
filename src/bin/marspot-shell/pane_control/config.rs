//! The numbers and switches the control socket follows: `control_*`
//! keys in `settings.toml` (re-read whenever L1 sees the file change).

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

/// The idle footprint of the largest helper measured: a claude on the
/// host with its tool servers, about 440 MiB resident.  A codex or kimi
/// helper, or one in a container, measured less.
const HELPER_IDLE_BYTES: u64 = 448 << 20;

/// The most shadow panes at once on a machine of `mem_bytes`: as many
/// idle helpers as fit in a quarter of its memory, at least 4, at most 24.
pub(super) fn shadows_for_memory(mem_bytes: u64) -> usize {
    usize::try_from(mem_bytes / 4 / HELPER_IDLE_BYTES).unwrap_or(usize::MAX).clamp(4, 24)
}

fn physical_memory() -> u64 {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: hw.memsize is a u64 and `len` says how big the buffer is
    let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&raw mut bytes).cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc == 0 { bytes } else { 0 }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            shadows_total: shadows_for_memory(physical_memory()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadows_total_follows_the_machine_memory() {
        let gib = |n: u64| n << 30;
        assert_eq!(shadows_for_memory(gib(8)), 4);
        assert_eq!(shadows_for_memory(gib(16)), 9);
        assert_eq!(shadows_for_memory(gib(32)), 18);
        assert_eq!(shadows_for_memory(gib(64)), 24);
        assert_eq!(shadows_for_memory(gib(512)), 24);
        assert_eq!(shadows_for_memory(0), 4, "memory not read");
        assert!(physical_memory() > gib(1));
        let cfg = Config::default();
        assert!((4..=24).contains(&cfg.shadows_total));
        assert_eq!(Config::parse("control_shadows_total = 40\n").shadows_total, 40, "settings still decide");
    }
}
