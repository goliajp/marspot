//! Main-loop stall detector.
//!
//! Both L2 and L3 are single-threaded event loops, and both have now
//! shipped bugs of the same shape: a blocking operation parked on the
//! loop, freezing everything downstream of it until the operation
//! finished.  Two were found in one day — a blocking `write(2)` to a
//! PTY master (seconds to minutes) and a 50 MiB bytelog compaction
//! (10+ seconds) — and in both cases the log was no help.  Sessions
//! only log on events, so a wedged loop and an idle loop leave exactly
//! the same trace: nothing.  Diagnosis came down to catching the
//! process in the act and reading a stack.
//!
//! `LoopWatch` closes that gap.  It times each loop iteration, splits
//! it into named phases, and emits a report only when an iteration
//! runs past a threshold — naming the phase that ate the time.  Idle
//! loops stay silent, so this costs nothing in the common case: two
//! `Instant::now()` calls per phase and no allocation after
//! construction.
//!
//! It deliberately does not decide *what* to do about a stall.  It
//! reports; the caller logs.  Keeping the logging macro out of here is
//! what lets L2 and L3 share it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum phases tracked per iteration.  Phases past this are folded
/// into the last slot rather than dropped, so the total stays honest
/// even if a caller over-instruments.
const MAX_PHASES: usize = 12;

/// What a stalled iteration spent its time on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallReport {
    /// Wall time for the whole iteration.
    pub total: Duration,
    /// Name of the phase that took longest.
    pub slowest: &'static str,
    /// How long that phase took.
    pub slowest_took: Duration,
    /// Every phase that ran, in order, with its duration.  Lets a
    /// caller log the full breakdown when one phase isn't the whole
    /// story.
    pub phases: Vec<(&'static str, Duration)>,
}

/// State the watchdog thread reads while the loop is still inside an
/// iteration.
///
/// Cheap to update: two relaxed atomic stores per iteration and one
/// uncontended lock per phase.  The loop iterates per event, not per
/// byte, so this is nowhere near the per-byte or per-frame budgets.
struct WatchShared {
    /// The clock both sides measure against.
    epoch: Instant,
    /// Nanos since this watch was created, marking when the current
    /// iteration began.  `0` means "between iterations" — the loop is
    /// parked waiting for work, which is not a stall.
    started_nanos: AtomicU64,
    /// Bumped on every `begin`, so the watchdog warns at most once per
    /// iteration no matter how long it runs.
    generation: AtomicU64,
    /// Name of the phase currently executing.
    phase: Mutex<&'static str>,
}

pub struct LoopWatch {
    threshold: Duration,
    epoch: Instant,
    shared: Arc<WatchShared>,
    iter_start: Instant,
    phase_start: Instant,
    current: &'static str,
    /// Fixed-capacity accumulator — reused across iterations so the
    /// steady state allocates nothing.
    phases: [(&'static str, Duration); MAX_PHASES],
    n_phases: usize,
    /// Iterations that ran past the threshold since construction.
    /// Cheap to expose and useful in a periodic summary.
    stalls: u64,
}

impl LoopWatch {
    /// `threshold` is the shortest iteration worth reporting.  Pick it
    /// above normal work but far below what a user notices as a freeze:
    /// a loop that services keystrokes should be well under a frame,
    /// so anything past ~100 ms is already anomalous.
    pub fn new(threshold: Duration) -> Self {
        let now = Instant::now();
        Self {
            threshold,
            epoch: now,
            shared: Arc::new(WatchShared {
                epoch: now,
                started_nanos: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                phase: Mutex::new("start"),
            }),
            iter_start: now,
            phase_start: now,
            current: "start",
            phases: [("", Duration::ZERO); MAX_PHASES],
            n_phases: 0,
            stalls: 0,
        }
    }

    /// Start a fresh iteration.  Call this *after* the loop's blocking
    /// wait returns — time spent parked waiting for work is not a
    /// stall, it's the loop doing its job.
    pub fn begin(&mut self) {
        let now = Instant::now();
        self.iter_start = now;
        self.phase_start = now;
        self.current = "start";
        self.n_phases = 0;
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
        *self.shared.phase.lock().unwrap_or_else(|p| p.into_inner()) = "start";
        let since_epoch = now.duration_since(self.epoch).as_nanos() as u64;
        // Never store 0 — that is the "between iterations" sentinel.
        self.shared
            .started_nanos
            .store(since_epoch.max(1), Ordering::Relaxed);
    }

    /// Close the current phase and open `name`.
    pub fn phase(&mut self, name: &'static str) {
        let now = Instant::now();
        self.push(self.current, now.duration_since(self.phase_start));
        self.current = name;
        self.phase_start = now;
        *self.shared.phase.lock().unwrap_or_else(|p| p.into_inner()) = name;
    }

    fn push(&mut self, name: &'static str, took: Duration) {
        if self.n_phases < MAX_PHASES {
            self.phases[self.n_phases] = (name, took);
            self.n_phases += 1;
        } else {
            // Over-instrumented: fold into the last slot so the sum of
            // the phases still accounts for the whole iteration.
            let last = &mut self.phases[MAX_PHASES - 1];
            last.1 += took;
        }
    }

    /// End the iteration.  Returns a report only if it ran past the
    /// threshold; `None` — the overwhelmingly common case — means the
    /// caller does nothing and nothing is logged.
    pub fn end(&mut self) -> Option<StallReport> {
        let now = Instant::now();
        self.shared.started_nanos.store(0, Ordering::Relaxed);
        self.push(self.current, now.duration_since(self.phase_start));
        let total = now.duration_since(self.iter_start);
        if total < self.threshold {
            return None;
        }
        self.stalls += 1;
        let used = &self.phases[..self.n_phases];
        // Every phase carries a name: `begin` seeds `"start"` and every
        // other entry comes from a `&'static str` the caller passed, so
        // there is no empty-name case to fold.  (An earlier version
        // guarded for one — the guard was unreachable and the comment
        // described a state that cannot occur.)
        let (slowest, slowest_took) = used
            .iter()
            .max_by_key(|(_, d)| *d)
            .copied()
            .unwrap_or(("unknown", total));
        Some(StallReport {
            total,
            slowest,
            slowest_took,
            phases: used.to_vec(),
        })
    }

    /// Iterations that have run past the threshold so far.
    pub fn stall_count(&self) -> u64 {
        self.stalls
    }

    /// Watch for a stall that is **still happening**, from another
    /// thread.
    ///
    /// `end()` can only report a stall once the iteration finishes, and
    /// the two field incidents this detector was built for were both
    /// invisible for exactly that reason: while a pane was frozen the
    /// log said nothing, and diagnosis meant catching the process live
    /// and reading a stack.  Worse, a stall that ends by *leaving* the
    /// loop (session exit — which is how a blocked PTY write commonly
    /// unblocks) skips `end()` altogether and is never reported at all.
    ///
    /// This thread polls the shared iteration clock and fires
    /// `on_stall(elapsed, phase)` once per iteration that outlives
    /// `threshold`, so a live freeze shows up in the log *while the user
    /// is still staring at it*.
    ///
    /// The thread ends when the `LoopWatch` is dropped (it holds only a
    /// `Weak`), so a caller never has to shut it down.
    pub fn spawn_watchdog<F>(&self, on_stall: F)
    where
        F: Fn(Duration, &'static str) + Send + 'static,
    {
        let weak = Arc::downgrade(&self.shared);
        let threshold = self.threshold;
        // Poll fast enough that a "the pane is frozen" report and the
        // log line land in the same breath, cheap enough to be free:
        // one atomic load per tick on an otherwise sleeping thread.
        let tick = (threshold / 4).max(Duration::from_millis(25));
        std::thread::Builder::new()
            .name("loop-watchdog".into())
            .spawn(move || {
                let mut warned_for: u64 = 0;
                loop {
                    std::thread::sleep(tick);
                    let Some(shared) = weak.upgrade() else {
                        return; // owner dropped
                    };
                    let started = shared.started_nanos.load(Ordering::Relaxed);
                    if started == 0 {
                        continue; // parked between iterations
                    }
                    let iter_gen = shared.generation.load(Ordering::Relaxed);
                    if iter_gen == warned_for {
                        continue; // already reported this one
                    }
                    // `started` is nanos since the watch's epoch; the
                    // elapsed time is measured against the same clock.
                    let now_nanos = Instant::now()
                        .saturating_duration_since(shared.epoch)
                        .as_nanos() as u64;
                    let elapsed = Duration::from_nanos(now_nanos.saturating_sub(started));
                    if elapsed >= threshold {
                        warned_for = iter_gen;
                        let phase =
                            *shared.phase.lock().unwrap_or_else(|p| p.into_inner());
                        on_stall(elapsed, phase);
                    }
                }
            })
            .expect("spawn loop-watchdog thread");
    }
}

impl StallReport {
    /// One-line summary for the log message.
    ///
    /// Both loops used to inline `"main loop iteration took {:.2}s"`,
    /// which renders L2's 80 ms threshold as a flat `0.08s` — not enough
    /// resolution to tell an 80 ms hitch from a 140 ms one.  Scale the
    /// unit to the magnitude instead, and keep the wording in one place.
    pub fn summary(&self) -> String {
        format!("main loop iteration took {}", fmt_dur(self.total))
    }

    /// `pump 9.61s, snapshot 4ms, publish 1ms` — compact enough for one
    /// log field, ordered as the iteration ran.
    pub fn breakdown(&self) -> String {
        let mut s = String::new();
        for (i, (name, took)) in self.phases.iter().enumerate() {
            if took.is_zero() {
                continue;
            }
            if i > 0 && !s.is_empty() {
                s.push_str(", ");
            }
            s.push_str(name);
            s.push(' ');
            s.push_str(&fmt_dur(*took));
        }
        s
    }
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us >= 1_000_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else if us >= 1_000 {
        format!("{}ms", us / 1_000)
    } else {
        format!("{us}us")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn quiet_iterations_report_nothing() {
        let mut w = LoopWatch::new(Duration::from_millis(50));
        for _ in 0..5 {
            w.begin();
            w.phase("drain");
            w.phase("pump");
            assert!(w.end().is_none());
        }
        assert_eq!(w.stall_count(), 0);
    }

    #[test]
    fn a_slow_phase_is_named() {
        let mut w = LoopWatch::new(Duration::from_millis(30));
        w.begin();
        w.phase("drain");
        w.phase("pump");
        sleep(Duration::from_millis(60));
        w.phase("publish");
        let r = w.end().expect("should have reported a stall");
        assert_eq!(r.slowest, "pump", "breakdown was {}", r.breakdown());
        assert!(r.total >= Duration::from_millis(60));
        assert!(r.slowest_took >= Duration::from_millis(60));
        assert_eq!(w.stall_count(), 1);
        assert!(r.breakdown().contains("pump"));
    }

    /// The blocking wait before an iteration must not count against
    /// it — otherwise every idle loop reports a stall forever.
    #[test]
    fn time_before_begin_is_not_charged() {
        let mut w = LoopWatch::new(Duration::from_millis(30));
        sleep(Duration::from_millis(60)); // stands in for recv_timeout
        w.begin();
        w.phase("work");
        assert!(w.end().is_none());
    }

    /// Phases sum to the iteration total, including when a caller
    /// pushes more phases than the fixed accumulator holds.
    #[test]
    fn phases_account_for_the_whole_iteration() {
        let mut w = LoopWatch::new(Duration::ZERO);
        w.begin();
        for _ in 0..(MAX_PHASES * 2) {
            w.phase("x");
        }
        sleep(Duration::from_millis(5));
        let r = w.end().unwrap();
        let sum: Duration = r.phases.iter().map(|(_, d)| *d).sum();
        assert!(
            sum <= r.total && r.total - sum < Duration::from_millis(2),
            "phases summed to {sum:?} against a total of {:?}",
            r.total
        );
        assert!(r.phases.len() <= MAX_PHASES);
    }

    /// The watchdog must report a stall that has NOT finished.
    ///
    /// This is the whole reason it exists: `end()` cannot speak until
    /// the iteration completes, so a pane frozen right now produces an
    /// empty log — which is exactly what made the two field incidents
    /// so expensive to diagnose.
    #[test]
    fn watchdog_reports_a_stall_that_is_still_in_progress() {
        use std::sync::mpsc::channel;
        let mut w = LoopWatch::new(Duration::from_millis(100));
        let (tx, rx) = channel();
        w.spawn_watchdog(move |elapsed, phase| {
            let _ = tx.send((elapsed, phase));
        });

        w.begin();
        w.phase("blocking-write");
        // Simulate an iteration that is still stuck — note we never
        // call `end()` before checking.
        let got = rx.recv_timeout(Duration::from_secs(3));
        assert!(
            got.is_ok(),
            "watchdog stayed silent while an iteration was still running \
             past its threshold"
        );
        let (elapsed, phase) = got.unwrap();
        assert!(elapsed >= Duration::from_millis(100), "elapsed {elapsed:?}");
        assert_eq!(phase, "blocking-write", "must name the stuck phase");
        let _ = w.end();
    }

    /// It must fire at most once per iteration, not every poll.
    #[test]
    fn watchdog_reports_each_stall_once() {
        use std::sync::mpsc::channel;
        let mut w = LoopWatch::new(Duration::from_millis(60));
        let (tx, rx) = channel();
        w.spawn_watchdog(move |e, p| {
            let _ = tx.send((e, p));
        });
        w.begin();
        w.phase("stuck");
        std::thread::sleep(Duration::from_millis(500));
        let _ = w.end();
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, 1, "expected exactly one report for one stalled iteration");
    }

    /// A loop parked waiting for work is not a stall.
    #[test]
    fn watchdog_stays_quiet_between_iterations() {
        use std::sync::mpsc::channel;
        let mut w = LoopWatch::new(Duration::from_millis(60));
        let (tx, rx) = channel();
        w.spawn_watchdog(move |e, p| {
            let _ = tx.send((e, p));
        });
        w.begin();
        w.phase("quick");
        let _ = w.end();
        // Now idle — the loop is between iterations.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            rx.try_recv().is_err(),
            "watchdog fired while the loop was parked, not stalled"
        );
    }

    #[test]
    fn durations_format_readably() {
        assert_eq!(fmt_dur(Duration::from_micros(250)), "250us");
        assert_eq!(fmt_dur(Duration::from_millis(12)), "12ms");
        assert_eq!(fmt_dur(Duration::from_millis(9_610)), "9.61s");
    }
}
