//! The supervisor's liveness deadlines and crash budget: how long a core
//! may take to say hello and answer a ping, and when to stop restarting it.

use super::*;

/// How long after spawn we expect HELLO_ACK before declaring the core
/// hung at startup. Generous enough to cover a cold-cache first-run
/// of a freshly-built binary: macOS Gatekeeper provenance check on
/// the just-promoted current/marspot-core (~400 ms when warm, more
/// on cold cache) PLUS 9 cold L3 spawns (each another fresh-exec
/// provenance check) PLUS ShelldClient handshake PLUS list_sessions/
/// attach RPC roundtrips. 5 s used to be enough, but install-local's
/// silent update on a fresh release build occasionally tripped past
/// it (observed on 0.2.10 → 0.2.11), so bump to 15 s. A *real* hang
/// is still caught — the PONG_DEADLINE (15 s × 3 PING_INTERVALs) is
/// the steady-state liveness check.
pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// How often we issue PING.
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(5);
/// PONG must arrive within this many `PING_INTERVAL`s before we call
/// the core hung.  3 = 15 s, which gives plenty of slack for a busy
/// terminal session without making a real hang feel sticky.
pub(crate) const PONG_DEADLINE: Duration = Duration::from_secs(15);
/// A supervisor tick that arrives this long after the previous one
/// means **we** stopped running — the machine slept, a build storm
/// took every core, the main thread sat in AppKit.  Silence measured
/// across such a gap says nothing about the core, because nobody was
/// listening; a watchdog has to know whether it was awake before it
/// accuses anyone else of being asleep.  Deadlines are handed the gap
/// back rather than charged for it.
///
/// Measured past the moment the clock was told to tick, which while
/// awake is never more than a ping interval away: far outside ordinary
/// jitter, far inside a real hang.
pub(crate) const SUPERVISOR_STALL_GAP: Duration = PING_INTERVAL;
/// Crash-budget window.  More than `MAX_CRASHES_IN_WINDOW` in this
/// span and we stop auto-restarting (binary is broken; user needs
/// to roll back or reinstall).
pub(crate) const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 min
pub(crate) const MAX_CRASHES_IN_WINDOW: usize = 3;

/// Has this core missed its PONG deadline?
///
/// A free function so the rule can be stated once and tested without
/// an AppKit window: the whole 2026-08-11 cascade was this predicate
/// being fed an anchor that predated the core's ability to answer.
pub(crate) fn pong_overdue(hello_acked: bool, last_pong_at: Instant, now: Instant) -> bool {
    hello_acked && now.duration_since(last_pong_at) > PONG_DEADLINE
}

/// Move a deadline anchor forward by a stall we didn't witness,
/// never past `now` — the core gets the time back, but no credit it
/// hasn't earned yet.
pub(crate) fn forgive_stall(anchor: Instant, gap: Duration, now: Instant) -> Instant {
    (anchor + gap).min(now)
}

/// How long to stay hands-off after the crash budget trips.
///
/// One `CRASH_WINDOW`, doubling per trip up to 8×.  The first wait is
/// the natural unit: after `CRASH_WINDOW` of quiet the crash record
/// would have aged out anyway, so trying again then is exactly as
/// safe as never having tripped.  The doubling is what stops a
/// genuinely broken binary turning this into a slow loop — four
/// restarts per five minutes, then per ten, per twenty, per forty,
/// and there it stays.
pub(crate) fn restart_cooldown(trips: u32) -> Duration {
    CRASH_WINDOW * (1 << trips.saturating_sub(1).min(3))
}

#[cfg(test)]
mod supervisor_deadline_tests;
