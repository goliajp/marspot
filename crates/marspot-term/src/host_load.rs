//! Whether a timing assertion on this host can mean anything right now.
//!
//! A test that asserts "this took less than N milliseconds" measures
//! two things: the code, and the machine it ran on. On a dev box or a
//! runner doing nothing else those are the same measurement. On a
//! shared host they are not, and the assertion reports the host.
//!
//! That is not a hypothetical. In one day the suite went red six times
//! across two machines, never the same tests twice, every one of them
//! passing on its own afterwards -- while the hosts sat at load 33 to
//! 121. The cost is not the reruns. It is that a red stops meaning
//! "something regressed", and a gate nobody believes is not a gate.
//!
//! So a timing assertion asks first, and says plainly when it is
//! declining to measure. A skip that announces itself is a gap you can
//! see; a budget quietly widened until nothing fails is not.
//!
//! The real performance numbers do not come from here. They come from
//! `bin/bench-remote.sh`, which holds the host's exclusive lock, runs
//! on an idle machine and judges against a recorded baseline. What
//! stays in the test suite is an order-of-magnitude sentinel: it
//! catches a change that made something ten times slower, and says
//! nothing about eight percent.

/// The load average the host reports over the last minute.
///
/// `None` when the kernel will not say, which is treated as "no reason
/// to decline" -- a host that cannot be asked is not known to be busy.
pub fn load1() -> Option<f64> {
    let mut avg = [0f64; 3];
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    (n > 0).then_some(avg[0])
}

/// Above this, a timing assertion is measuring the host.
///
/// Twenty is the same number `bin/bench-remote.sh` refuses to measure
/// above, and one number is better than two that drift apart. A suite
/// running its own tests in parallel on an idle machine sits well
/// under it; the days this was written for were 33 to 121.
pub const BUSY: f64 = 20.0;

/// True when a timing assertion may be made, after saying why not.
///
/// `what` names the thing being timed, so a skipped run reads as a
/// sentence rather than as silence.
pub fn quiet_enough_to_time(what: &str) -> bool {
    match load1() {
        Some(l) if l > BUSY => {
            eprintln!(
                "skipping the timing assertion for {what}: host at load {l:.1}, \
                 over {BUSY:.0}. What it would measure is the machine. The \
                 numbers that count come from bin/bench-remote.sh on an idle \
                 host."
            );
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing worth pinning: a host that cannot be asked does
    /// not block the assertion. Written as a test because the opposite
    /// -- treating "unknown" as "busy" -- would turn every timing
    /// assertion off on a platform where the call is missing, and
    /// nothing would say so.
    #[test]
    fn an_unknown_load_does_not_decline_to_measure() {
        // `load1` answers on both platforms this builds for, so the
        // branch is exercised through the match arm rather than by
        // faking the syscall.
        assert!(load1().is_some(), "both platforms report a load average");
        let l = load1().unwrap();
        assert!(l >= 0.0, "a load average is not negative: {l}");
        assert_eq!(
            quiet_enough_to_time("a probe"),
            l <= BUSY,
            "the answer is the threshold and nothing else"
        );
    }
}
