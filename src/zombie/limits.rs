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
}

/// `zombie.notify` and the thresholds in a settings file; anything
/// missing or unreadable keeps its default.  `live` is not a mode of
/// this build and reads as `dryrun`, as does any other word.
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
            notify = if matches!(v.to_ascii_lowercase().as_str(), "off" | "false") { Notify::Off } else { Notify::DryRun };
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
        for word in ["live", "dryrun", "on", "maybe"] {
            assert_eq!(parse(&format!("zombie.notify = {word}\n")).0, Notify::DryRun, "{word}");
        }
    }
}
