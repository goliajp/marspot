//! Update state machine for the shell supervisor.
//!
//! The shell is the *supervisor* in the silent-update architecture: it owns
//! the window and never restarts, the core is the worker that gets swapped
//! on every upgrade. The filesystem surface (the three-slot `binaries/`
//! tree) lives in `marspot_term::binary_tree::BinaryTree` so the daemon
//! (`marspot-shelld`) can share it without pulling in any GUI deps; this
//! module just keeps the shell-side state machine.
//!
//! ## Lifecycle (post dual-core retirement, RFC-003 single-core swap)
//!
//! ```text
//!  Idle ──→ apply_pending_update():
//!            promote pending/→current/
//!            shutdown_active (old core)
//!            spawn_core (new active from current/)
//!         ──→ Idle (immediately; no probation gate)
//! ```
//!
//! The old dual-core probation was retired with RFC-003: L3 is single-
//! client, so a parallel pending core would kick the active core off
//! every L3's UDS the moment it shook hands, dropping all input for the
//! probation window.  L3 self-execv + state.bin reattach already lets
//! each L3 survive an L2 swap on its own, so the warm-up net was both
//! wrong-shape and unnecessary.  Now the swap is a brief blackout (~200
//! ms) while the new core boots and reattaches via the registry.

// Re-export so existing `supervisor::BinaryTree::for_shell()` call sites in
// `marspot-shell/main.rs` keep resolving without a churn rename.
pub use marspot::binary_tree::BinaryTree;

/// What one probe round concluded, per layer.  `None` means that layer
/// had nothing staged and was not probed.
///
/// Both layers are probed in the same round when both have a candidate,
/// which is the common case: `install-local.sh` stages shell and core
/// together.  Knowing about the core *before* the shell execs is what
/// lets the successor spawn the new core directly — otherwise it boots
/// the old one, and a second probe + swap retires it moments later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeVerdict {
    pub shell: Option<bool>,
    pub core: Option<bool>,
}

impl ProbeVerdict {
    /// Compact rendering for the log line: `shell=ok core=failed`.
    pub fn summary(&self) -> String {
        fn one(v: Option<bool>) -> &'static str {
            match v {
                Some(true) => "ok",
                Some(false) => "failed",
                None => "-",
            }
        }
        format!("shell={} core={}", one(self.shell), one(self.core))
    }
}

/// Where in the swap lifecycle the shell currently is. Lives on `ShellApp`;
/// transitions driven by either explicit user action (SIGUSR1 trigger) or
/// focus-loss.
///
/// `Probing` exists because a staged binary's very first `exec` is where
/// macOS charges the Gatekeeper assessment, and that charge has no upper
/// bound: on 2026-07-29 a concurrent cargo build flooded `syspolicyd` and
/// `marspot-core` sat in the kernel's exec path for 204 s.  The swap had
/// already killed the outgoing core by then, so the window was frozen for
/// the whole stall with no core to draw it.
///
/// So the probe runs *first*, on a background thread, against the still
/// un-promoted `pending/` file — a throwaway `--version` exec whose only
/// job is to make the kernel pay that bill while the outgoing process is
/// still on screen.  `promote_pending` uses `rename`, which preserves the
/// inode, so the verdict the probe warmed is the one the real spawn hits.
/// A slow Gatekeeper now means "the update lands later", not "the window
/// is frozen" — which is why there is deliberately no timeout here.
#[derive(Debug)]
pub enum SupervisorState {
    /// No update in flight; supervisor accepts new SIGUSR1 / focus-loss
    /// triggers.  Every completed or abandoned swap returns here.
    Idle,
    /// A background thread is exec'ing the staged binaries to warm
    /// their Gatekeeper verdicts.  Further triggers are ignored until
    /// it reports — the outgoing processes keep serving throughout.
    Probing,
}

/// What to do with one probe round's verdict, decided apart from doing
/// it so the rule can be read and tested in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbePlan {
    /// A candidate that failed leaves `pending/`, or the next trigger
    /// retries the same dead binary forever.
    pub quarantine_shell: bool,
    pub quarantine_core: bool,
    /// Both passed: the core goes into `current/` before the shell
    /// execs, so the successor spawns the new core once instead of
    /// booting the old one and swapping it seconds later.
    pub promote_core_before_exec: bool,
    pub then: AfterProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterProbe {
    /// Exec the new shell; it takes any promoted core with it.
    ShellExec,
    /// Only the core is new: swap it under the running shell.
    CoreSwap,
    /// Nothing passed: the running processes stay as they are.
    Nothing,
}

impl ProbeVerdict {
    pub fn plan(&self) -> ProbePlan {
        let both = self.shell == Some(true) && self.core == Some(true);
        ProbePlan {
            quarantine_shell: self.shell == Some(false),
            quarantine_core: self.core == Some(false),
            promote_core_before_exec: both,
            then: if self.shell == Some(true) {
                AfterProbe::ShellExec
            } else if self.core == Some(true) {
                AfterProbe::CoreSwap
            } else {
                AfterProbe::Nothing
            },
        }
    }
}

/// Record a crash at `now` and forget the ones older than `window`.
/// Returns how many are left in the window, the number the crash
/// budget is checked against.
pub fn note_crash(
    crashes: &mut std::collections::VecDeque<std::time::Instant>,
    now: std::time::Instant,
    window: std::time::Duration,
) -> usize {
    crashes.push_back(now);
    while crashes
        .front()
        .is_some_and(|t| now.duration_since(*t) > window)
    {
        crashes.pop_front();
    }
    crashes.len()
}

/// The window frame a predecessor shell handed over in
/// `MARSPOT_RESTORE_FRAME` as `x,y,w,h`, or `None` unless it is exactly
/// four finite numbers with a positive size.  A field that does not
/// parse rejects the whole frame: dropping it would shift the rest
/// into the wrong places.
pub fn parse_restore_frame(s: &str) -> Option<(f64, f64, f64, f64)> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>().ok().filter(|n| n.is_finite()))
        .collect::<Option<_>>()?;
    match v[..] {
        [x, y, w, h] if w > 0.0 && h > 0.0 => Some((x, y, w, h)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    fn plan(shell: Option<bool>, core: Option<bool>) -> ProbePlan {
        ProbeVerdict { shell, core }.plan()
    }

    /// Every combination of the two layers' outcomes, against the rule:
    /// failures are quarantined, a good shell execs (taking a good core
    /// with it), a good core alone is swapped, nothing else moves.
    #[test]
    fn a_probe_round_routes_each_layer_by_its_own_verdict() {
        use AfterProbe::*;
        let cases = [
            // shell, core, quarantine shell, quarantine core, promote, then
            (None, None, false, false, false, Nothing),
            (Some(true), None, false, false, false, ShellExec),
            (None, Some(true), false, false, false, CoreSwap),
            (Some(true), Some(true), false, false, true, ShellExec),
            (Some(false), None, true, false, false, Nothing),
            (None, Some(false), false, true, false, Nothing),
            (Some(false), Some(false), true, true, false, Nothing),
            (Some(false), Some(true), true, false, false, CoreSwap),
            (Some(true), Some(false), false, true, false, ShellExec),
        ];
        for (shell, core, qs, qc, promote, then) in cases {
            assert_eq!(
                plan(shell, core),
                ProbePlan {
                    quarantine_shell: qs,
                    quarantine_core: qc,
                    promote_core_before_exec: promote,
                    then,
                },
                "shell={shell:?} core={core:?}"
            );
        }
    }

    /// A probe thread that died says nothing, and nothing may be
    /// promoted or quarantined on its account.
    #[test]
    fn an_empty_verdict_moves_nothing() {
        let p = ProbeVerdict::default().plan();
        assert!(!p.quarantine_shell && !p.quarantine_core && !p.promote_core_before_exec);
        assert_eq!(p.then, AfterProbe::Nothing);
        assert_eq!(ProbeVerdict::default().summary(), "shell=- core=-");
        let mixed = ProbeVerdict {
            shell: Some(true),
            core: Some(false),
        };
        assert_eq!(mixed.summary(), "shell=ok core=failed");
    }

    /// Crashes count inside the window and age out of it; one exactly
    /// a window old still counts.
    #[test]
    fn crashes_count_only_inside_the_window() {
        let w = Duration::from_secs(300);
        let t0 = Instant::now();
        let mut c = VecDeque::new();
        assert_eq!(note_crash(&mut c, t0, w), 1);
        assert_eq!(note_crash(&mut c, t0 + Duration::from_secs(100), w), 2);
        assert_eq!(note_crash(&mut c, t0 + w, w), 3, "exactly a window old still counts");
        assert_eq!(
            note_crash(&mut c, t0 + w + Duration::from_secs(1), w),
            3,
            "the first one has aged out"
        );
        assert_eq!(note_crash(&mut c, t0 + w * 10, w), 1, "a quiet stretch forgets them all");
    }

    #[test]
    fn a_restore_frame_is_four_finite_numbers_with_a_size() {
        assert_eq!(parse_restore_frame("10,20,800,600"), Some((10.0, 20.0, 800.0, 600.0)));
        assert_eq!(
            parse_restore_frame("-1440.5, 0, 1.5, 2"),
            Some((-1440.5, 0.0, 1.5, 2.0))
        );
        for bad in [
            "",
            "10,20,800",
            "10,20,800,600,1",
            "10,20,0,600",
            "10,20,800,-1",
            "10,x,800,600",
            "10,,20,800,600",
            "10,20,inf,600",
            "NaN,20,800,600",
            "10;20;800;600",
        ] {
            assert_eq!(parse_restore_frame(bad), None, "{bad:?}");
        }
    }
}
