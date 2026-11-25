//! The watch's `zombie.*` keys in settings.toml.
//!
//! The shell reads them itself on every check, so the shared settings
//! file format keeps no fields of its own for them; a key this build
//! does not know is left alone, as everywhere in that file.

use super::rules::{HOUR, Limits, MINUTE};

/// What the watch does with a suspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// Nothing: no check runs.
    Off,
    /// Log each suspect and the message it would send; send nothing.
    DryRun,
    /// Log each suspect and send the message to the session's inbox.
    Live,
}

/// `zombie.notify` and the thresholds in a settings file; anything
/// missing or unreadable keeps its default.  Only `live` sends;
/// any word other than `off` and `live` reads as `dryrun`.
pub fn parse(body: &str) -> (Notify, Limits) {
    let mut notify = Notify::DryRun;
    let mut l = Limits::default();
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        let Some(k) = k.strip_prefix("zombie.") else { continue };
        if k == "notify" {
            notify = match v.to_ascii_lowercase().as_str() {
                "off" | "false" => Notify::Off,
                "live" => Notify::Live,
                _ => Notify::DryRun,
            };
            continue;
        }
        let Ok(n) = v.parse::<u64>() else { continue };
        let slot = match k {
            "b1_silent_minutes" => (&mut l.b1_silent, MINUTE),
            "b1_cpu_ms" => (&mut l.b1_cpu_ns, 1_000_000),
            "b1_window_minutes" => (&mut l.b1_window, MINUTE),
            "b3_grace_minutes" => (&mut l.b3_grace, MINUTE),
            "s1_quiet_minutes" => (&mut l.s1_quiet, MINUTE),
            "s1_idle_minutes" => (&mut l.s1_idle, MINUTE),
            "s2_extra_minutes" => (&mut l.s2_extra, MINUTE),
            "o1_stale_hours" => (&mut l.o1_stale, HOUR),
            "o2_age_hours" => (&mut l.o2_age, HOUR),
            "o2_cpu_ms" => (&mut l.o2_cpu_ns, 1_000_000),
            "o2_window_minutes" => (&mut l.o2_window, MINUTE),
            _ => continue,
        };
        *slot.0 = n.saturating_mul(slot.1);
    }
    (notify, l)
}

/// `zombie.scale_for_tests`: every time of the watch divided by this,
/// so a window test sees in seconds what takes hours.  1 when unset or
/// unreadable; the shell honours it only in a sandbox.  Kept within
/// 1..=[`SCALE_MAX`]: the shell divides its five-minute tick by it, as
/// a u32, and a tick of 0 ms would spin.
pub fn scale(body: &str) -> u64 {
    body.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == "zombie.scale_for_tests")
        .filter_map(|(_, v)| v.trim().trim_matches('"').parse::<u64>().ok())
        .next_back()
        .unwrap_or(1)
        .clamp(1, SCALE_MAX)
}

pub const SCALE_MAX: u64 = 3600;

impl Limits {
    /// Every time divided by `k`, at least a second; CPU budgets stay.
    pub fn scaled(&self, k: u64) -> Limits {
        let k = k.max(1);
        let t = |v: u64| (v / k).max(1);
        Limits {
            b1_silent: t(self.b1_silent),
            b1_window: t(self.b1_window),
            b3_grace: t(self.b3_grace),
            s1_quiet: t(self.s1_quiet),
            s1_idle: t(self.s1_idle),
            s2_extra: t(self.s2_extra),
            s2_default_timeout: t(self.s2_default_timeout),
            o1_stale: t(self.o1_stale),
            o2_age: t(self.o2_age),
            o2_window: t(self.o2_window),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_settings_default_to_a_dry_run_with_the_designed_thresholds() {
        let (n, l) = parse("");
        assert_eq!(n, Notify::DryRun);
        assert_eq!(l, Limits::default());
        assert_eq!(l.b1_silent, 2 * HOUR);
        assert_eq!(l.s1_quiet, 30 * MINUTE);
    }

    #[test]
    fn zombie_settings_turn_off_and_change_a_threshold_and_keep_the_rest() {
        let (n, l) = parse("# c\nzombie.notify = off\nzombie.s1_quiet_minutes = 45\nzombie.o2_cpu_ms = x\nother = 1\n");
        assert_eq!(n, Notify::Off);
        assert_eq!(l.s1_quiet, 45 * MINUTE);
        assert_eq!(l.o2_cpu_ns, Limits::default().o2_cpu_ns, "an unreadable number keeps the default");
        for word in ["dryrun", "on", "maybe"] {
            assert_eq!(parse(&format!("zombie.notify = {word}\n")).0, Notify::DryRun, "{word}");
        }
        assert_eq!(parse("zombie.notify = \"live\"\n").0, Notify::Live);
    }

    #[test]
    fn zombie_settings_scale_divides_times_but_not_cpu() {
        assert_eq!(scale(""), 1);
        assert_eq!(scale("zombie.scale_for_tests = 0\n"), 1);
        assert_eq!(scale("# zombie.scale_for_tests = 9\nzombie.scale_for_tests = 60\n"), 60);
        assert_eq!(scale("zombie.scale_for_tests = 3600\n"), 3600);
        for big in ["3601", "300000", "18446744073709551615"] {
            assert_eq!(scale(&format!("zombie.scale_for_tests = {big}\n")), SCALE_MAX, "{big}");
        }
        let l = Limits::default().scaled(60);
        assert_eq!(l.b1_silent, 2 * MINUTE);
        assert_eq!(l.o1_stale, 24 * MINUTE);
        assert_eq!(l.b1_cpu_ns, Limits::default().b1_cpu_ns);
        assert_eq!(Limits::default().scaled(1), Limits::default());
    }
}
