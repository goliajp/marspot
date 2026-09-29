//! Non-blocking path-existence oracle for the render path.
//!
//! `build_instances` scans every rebuilt pane for clickable spans on
//! every frame, and deciding whether `/foo/bar` is a *file* link means
//! asking the filesystem.  Asking it **on the render thread** is the
//! defect this module exists to remove.
//!
//! ## Why a cache is not the fix
//!
//! The obvious reading is "too many syscalls, cache harder".  Measured
//! on the live 14-pane window (2026-08-22), it is not:
//! `examples/link_scan_probe` replaying each pane's real bytelog put
//! the whole window at **0–37 probes per scan**.  Volume was never the
//! problem.  Tail latency was:
//!
//! | probe                              | median | max      |
//! |------------------------------------|-------:|---------:|
//! | `~/nas/<missing>` (smb / tailscale)| 1.3 µs | **6.13 s** |
//! | `/Volumes/home/<missing>` (smb)    | 0.8 µs | **5.98 s** |
//! | `/home/kevybench/boxpre` (autofs)  |    —   | **8.4 ms** even on an idle host |
//! | `~/OrbStack/<missing>` (nfs)       | 1.3 µs | 15.7 ms  |
//! | local missing path (control)       | 0.8 µs | 123 µs   |
//!
//! One probe of a path that happens to live under a network mount —
//! or under macOS's `auto_home` autofs map, which any line mentioning
//! a Linux `/home/...` path reaches — blocks the caller for seconds.
//! Three of them in one frame is the 13.6 s `l2.loop.stall` the core
//! logged, and no cache size prevents the *first* one.
//!
//! The only fix is that the render thread never waits.  `probe()` is
//! a memory lookup that always answers immediately, returning
//! [`PathVerdict::Unknown`] on a miss and handing the path to a
//! worker thread.  The link appears a frame or two later, once the
//! answer lands — the scanner already treats `Unknown` as "not a link
//! yet", so nothing downstream needs a notion of pending.
//!
//! ## An answer, once held, is never withdrawn
//!
//! `Unknown` is for a path nobody has ever answered.  A lapsed TTL is
//! not that: it means "worth asking again", and the held verdict is
//! served while the refresh runs.  Answering `Unknown` there is what
//! made links come and go — see `lookup_or_queue` for the three steps
//! that left a pane sitting with its links missing until it was
//! focused.
//!
//! The property this buys is worth naming: **what is on screen
//! changes only when the filesystem's answer changes.**  No clock
//! takes a link away.
//!
//! ## Bounded, per the "cannot get slower the longer it runs" rule
//!
//! - cache: two generations of at most `CACHE_CAP` entries each.  When
//!   `hot` fills, it demotes to `cold` and a fresh `hot` starts; a
//!   lookup that hits `cold` is promoted back.  Never a wholesale
//!   `clear()` — the predecessor did exactly that at 256 entries and
//!   so threw away its live working set on a schedule.
//! - queue: `QUEUE_CAP` paths.  Overflow drops the request; the next
//!   frame asks again.  Nothing accumulates.
//! - a probe that took longer than `SLOW_PROBE` gets a much longer
//!   TTL.  A stalled mount answers slowly every time and its answer
//!   does not usefully change; re-paying seconds for it on a timer is
//!   the one thing worse than paying it once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marspot_term::grid_links::{FsOracle, PathOracle, PathVerdict};

/// Entries per cache generation; two generations are live at once.
const CACHE_CAP: usize = 4096;
/// Paths awaiting a worker probe.  Overflow drops, never blocks.
const QUEUE_CAP: usize = 512;
/// How long an ordinary verdict stays good.
const TTL: Duration = Duration::from_secs(5);
/// A probe slower than this came from a network / autofs path.
const SLOW_PROBE: Duration = Duration::from_millis(20);
/// TTL for those.  Long, deliberately — see the module docs.
const SLOW_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy)]
struct Entry {
    exists: bool,
    at: Instant,
    ttl: Duration,
}

impl Entry {
    fn fresh(&self, now: Instant) -> bool {
        now.duration_since(self.at) < self.ttl
    }
}

#[derive(Default)]
struct Cache {
    hot: HashMap<String, Entry>,
    cold: HashMap<String, Entry>,
    /// Queued or being probed right now — keeps one path from
    /// occupying several queue slots while the worker is on it.
    inflight: HashSet<String>,
    queue: VecDeque<String>,
    /// Set when the worker is asked to stop, so it can drain and exit
    /// rather than park forever on the condvar.
    stopping: bool,
}

pub struct LinkProbe {
    cache: Mutex<Cache>,
    work: Condvar,
    /// Bumped on every landed verdict.  The renderer folds this into
    /// its per-pane fingerprint, so a pane whose content did not
    /// change still rebuilds once when new link answers arrive —
    /// otherwise a resolved link would not paint until the next
    /// keystroke.
    generation: AtomicU64,
}

impl LinkProbe {
    fn new() -> Self {
        Self {
            cache: Mutex::new(Cache::default()),
            work: Condvar::new(),
            generation: AtomicU64::new(1),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Look the path up, and queue a probe when the answer is missing
    /// or stale.  Holds the lock only for map operations — never
    /// across a syscall.
    ///
    /// **A verdict we already hold is never withdrawn.**  A lapsed TTL
    /// means "worth asking again", not "no longer true", and answering
    /// `Unknown` on the way to re-confirming it is what put a pane on
    /// screen with its links missing:
    ///
    ///   1. the five-second TTL lapses; the next rebuild of that pane
    ///      asks, gets `Unknown`, and draws the paths as plain text;
    ///   2. the worker re-probes and gets *the same* answer, so
    ///      `changed` is false and the generation does not move —
    ///      correctly, because bumping it rebuilds all fourteen panes;
    ///   3. nothing therefore rebuilds that pane again, and it sits
    ///      there with no links until something unrelated disturbs it.
    ///
    /// Focusing the pane is one such disturbance, which is why the
    /// links came back when you clicked into it and why they seemed to
    /// come and go at random: whether a rebuild landed inside a lapse
    /// window decided what you saw. Serving the held answer removes
    /// the window — what is on screen only ever changes when the
    /// filesystem's answer actually changes.
    fn lookup_or_queue(&self, path: &str) -> PathVerdict {
        let now = Instant::now();
        let mut c = self.cache.lock().expect("link-probe cache poisoned");

        let held = match c.hot.get(path).copied() {
            Some(e) => Some(e),
            // A cold hit is a live entry that merely survived a
            // rotation; promote it so the next rotation does not lose
            // it.
            None => c.cold.get(path).copied().inspect(|e| {
                insert_hot(&mut c, path.to_string(), *e);
            }),
        };
        if let Some(e) = held {
            if e.fresh(now) {
                return verdict(e.exists);
            }
        }

        if !c.inflight.contains(path) && c.queue.len() < QUEUE_CAP {
            c.inflight.insert(path.to_string());
            c.queue.push_back(path.to_string());
            drop(c);
            self.work.notify_one();
        }
        // Stale but held: keep saying what we last knew while the
        // refresh runs.  Only a path we have never answered is
        // genuinely unknown.
        match held {
            Some(e) => verdict(e.exists),
            None => PathVerdict::Unknown,
        }
    }

    /// Worker body: take one path, probe it off-lock, record it.
    fn run(&self) {
        loop {
            let path = {
                let mut c = self.cache.lock().expect("link-probe cache poisoned");
                loop {
                    if c.stopping {
                        return;
                    }
                    match c.queue.pop_front() {
                        Some(p) => break p,
                        None => {
                            c = self.work.wait(c).expect("link-probe cache poisoned");
                        }
                    }
                }
            };

            // The expensive part, deliberately outside the lock: this
            // call is allowed to take six seconds.
            let t0 = Instant::now();
            let exists = matches!(FsOracle.probe(&path), PathVerdict::Exists);
            let took = t0.elapsed();

            let entry = Entry {
                exists,
                at: Instant::now(),
                ttl: if took >= SLOW_PROBE { SLOW_TTL } else { TTL },
            };
            let mut c = self.cache.lock().expect("link-probe cache poisoned");
            c.inflight.remove(&path);
            // Only a verdict the renderer has not already drawn is
            // worth a generation bump: the counter invalidates EVERY
            // pane's instance cache, so re-confirming the same answer
            // when a TTL lapses would rebuild all 14 panes on a timer
            // for no visible change.  A path we have never answered,
            // or one whose answer flipped, is the real signal.
            let changed = match c.hot.get(&path).or_else(|| c.cold.get(&path)) {
                Some(prev) => prev.exists != entry.exists,
                None => true,
            };
            insert_hot(&mut c, path, entry);
            drop(c);
            if changed {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn verdict(exists: bool) -> PathVerdict {
    if exists {
        PathVerdict::Exists
    } else {
        PathVerdict::Missing
    }
}

/// Insert into `hot`, rotating generations when it is full.  Two
/// bounded maps instead of one map plus an eviction order: the
/// working set here is a screenful of path-shaped tokens, so a
/// generation boundary loses at most the coldest half, and a hit in
/// `cold` promotes.  A real LRU would cost a linked list per entry to
/// protect a set this small.
fn insert_hot(c: &mut Cache, path: String, entry: Entry) {
    if c.hot.len() >= CACHE_CAP {
        c.cold = std::mem::take(&mut c.hot);
    }
    c.hot.insert(path, entry);
}

impl PathOracle for LinkProbe {
    fn probe(&self, path: &str) -> PathVerdict {
        self.lookup_or_queue(path)
    }
}

static PROBE: OnceLock<&'static LinkProbe> = OnceLock::new();

/// Start the worker and make [`oracle`] hand out the async probe.
/// Called once by `marspot-core` at boot.  Idempotent.
pub fn install() {
    PROBE.get_or_init(|| {
        let probe: &'static LinkProbe = Box::leak(Box::new(LinkProbe::new()));
        std::thread::Builder::new()
            .name("link-probe".into())
            .spawn(move || probe.run())
            .expect("spawn link-probe worker");
        probe
    });
}

/// The oracle the render path should use.
///
/// Falls back to the blocking [`FsOracle`] when nothing was installed
/// — that is the right answer for one-shot callers (tests, `mcli`,
/// `--snapshot`), which want a definite verdict on the first and only
/// frame they will ever draw, and which are not a render loop.
pub fn oracle() -> &'static dyn PathOracle {
    match PROBE.get() {
        Some(p) => *p as &'static dyn PathOracle,
        None => &FsOracle,
    }
}

/// Bumped whenever a verdict lands; folded into the per-pane
/// instance-cache fingerprint so resolved links repaint.
pub fn generation() -> u64 {
    PROBE.get().map_or(0, |p| p.generation())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The render-path contract: a first ask never blocks and never
    /// claims to know.
    #[test]
    fn first_ask_is_unknown_and_does_not_block() {
        let probe = LinkProbe::new();
        let t0 = Instant::now();
        let v = probe.probe("/definitely/not/here/at/all");
        assert_eq!(v, PathVerdict::Unknown);
        assert!(t0.elapsed() < Duration::from_millis(5), "probe blocked");
    }

    /// The reported defect, as a test.
    ///
    /// A link that has resolved must not un-resolve because a clock
    /// ran out.  Before this, a lapsed TTL answered `Unknown`, the
    /// pane rebuilt without its links, and — since re-confirming the
    /// same answer moves no generation — nothing rebuilt it again.
    /// The pane stayed link-less until it was focused.
    #[test]
    fn a_lapsed_verdict_is_still_served_while_it_is_rechecked() {
        let probe = LinkProbe::new();
        let path = "/some/answered/path";

        // Stand in for a landed verdict that has since gone stale.
        {
            let mut c = probe.cache.lock().unwrap();
            let stale = Entry {
                exists: true,
                at: Instant::now() - TTL - Duration::from_secs(1),
                ttl: TTL,
            };
            c.hot.insert(path.to_string(), stale);
        }

        assert_eq!(
            probe.probe(path),
            PathVerdict::Exists,
            "a lapsed TTL means 'ask again', not 'no longer true'"
        );
        // …and it did ask again.
        {
            let c = probe.cache.lock().unwrap();
            assert!(c.inflight.contains(path), "no refresh was queued");
        }
        // Asking twice does not queue it twice.
        assert_eq!(probe.probe(path), PathVerdict::Exists);
        {
            let c = probe.cache.lock().unwrap();
            assert_eq!(c.queue.len(), 1);
        }

        // The same holds for a path we last saw as missing: it stays
        // "not a link" rather than briefly becoming unknown, so the
        // scan's answer never changes without the filesystem's.
        let gone = "/some/answered/gone";
        {
            let mut c = probe.cache.lock().unwrap();
            c.hot.insert(
                gone.to_string(),
                Entry {
                    exists: false,
                    at: Instant::now() - TTL - Duration::from_secs(1),
                    ttl: TTL,
                },
            );
        }
        assert_eq!(probe.probe(gone), PathVerdict::Missing);

        // A path nobody ever answered is the only genuine unknown.
        assert_eq!(probe.probe("/never/asked"), PathVerdict::Unknown);
    }

    /// A stale entry that only lives in `cold` is served too — the
    /// rotation is a cache-size mechanism, not an expiry.
    #[test]
    fn a_lapsed_verdict_in_the_cold_generation_is_served_and_promoted() {
        let probe = LinkProbe::new();
        let path = "/rotated/out/path";
        {
            let mut c = probe.cache.lock().unwrap();
            c.cold.insert(
                path.to_string(),
                Entry {
                    exists: true,
                    at: Instant::now() - TTL - Duration::from_secs(1),
                    ttl: TTL,
                },
            );
        }
        assert_eq!(probe.probe(path), PathVerdict::Exists);
        let c = probe.cache.lock().unwrap();
        assert!(c.hot.contains_key(path), "a served cold entry is promoted");
    }

    /// Once the worker lands an answer the next ask is definite, and
    /// the generation moved so the renderer knows to repaint.
    #[test]
    fn worker_result_becomes_visible_and_bumps_generation() {
        let probe: &'static LinkProbe = Box::leak(Box::new(LinkProbe::new()));
        std::thread::spawn(move || probe.run());
        let g0 = probe.generation();

        assert_eq!(probe.probe("/"), PathVerdict::Unknown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && probe.probe("/") == PathVerdict::Unknown {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.probe("/"), PathVerdict::Exists);
        assert!(probe.generation() > g0);
    }

    /// A missing path is cached as missing, not left Unknown forever
    /// — otherwise every frame would re-queue every non-path token.
    #[test]
    fn missing_paths_are_cached_as_missing() {
        let probe: &'static LinkProbe = Box::leak(Box::new(LinkProbe::new()));
        std::thread::spawn(move || probe.run());
        let p = "/nope-marspot-link-probe-test";
        assert_eq!(probe.probe(p), PathVerdict::Unknown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && probe.probe(p) == PathVerdict::Unknown {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.probe(p), PathVerdict::Missing);
    }

    /// The queue is a bound, not a buffer: asking about far more
    /// distinct paths than it holds must not grow it.
    #[test]
    fn queue_is_bounded() {
        let probe = LinkProbe::new();
        for i in 0..(QUEUE_CAP * 4) {
            probe.probe(&format!("/bounded-probe-{i}"));
        }
        let c = probe.cache.lock().unwrap();
        assert!(c.queue.len() <= QUEUE_CAP, "queue grew to {}", c.queue.len());
        assert!(c.inflight.len() <= QUEUE_CAP);
    }

    /// Re-confirming a verdict the renderer already drew must not
    /// invalidate every pane — otherwise the instance cache is
    /// defeated on the TTL's schedule rather than by real change.
    #[test]
    fn unchanged_verdict_does_not_bump_generation() {
        let probe: &'static LinkProbe = Box::leak(Box::new(LinkProbe::new()));
        std::thread::spawn(move || probe.run());

        // First answer: new information, so it must bump.
        let g0 = probe.generation();
        assert_eq!(probe.probe("/"), PathVerdict::Unknown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && probe.probe("/") == PathVerdict::Unknown {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.probe("/"), PathVerdict::Exists);
        let g1 = probe.generation();
        assert!(g1 > g0, "a first answer must invalidate");

        // Age the entry out so the next ask re-queues it, and check
        // that landing the SAME answer changes nothing.
        {
            let mut c = probe.cache.lock().unwrap();
            let stale = Entry {
                exists: true,
                at: Instant::now() - TTL - Duration::from_secs(1),
                ttl: TTL,
            };
            c.hot.insert("/".into(), stale);
        }
        // The held answer is served throughout — the ask that starts
        // the refresh included.  Nothing on screen flickers while a
        // verdict is being re-confirmed.
        assert_eq!(probe.probe("/"), PathVerdict::Exists);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = {
                let c = probe.cache.lock().unwrap();
                !c.inflight.contains("/") && c.queue.is_empty()
            };
            if done || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.probe("/"), PathVerdict::Exists);
        assert_eq!(probe.generation(), g1, "re-confirmation must not invalidate");
    }

    /// Cache rotation keeps a live entry reachable instead of
    /// dropping the working set the way the old clear-at-256 did.
    #[test]
    fn rotation_promotes_instead_of_dropping() {
        let probe = LinkProbe::new();
        let live = Entry { exists: true, at: Instant::now(), ttl: TTL };
        {
            let mut c = probe.cache.lock().unwrap();
            insert_hot(&mut c, "/keep-me".into(), live);
            for i in 0..CACHE_CAP {
                insert_hot(&mut c, format!("/filler-{i}"), live);
            }
            assert!(!c.hot.contains_key("/keep-me"), "expected a rotation");
        }
        assert_eq!(probe.probe("/keep-me"), PathVerdict::Exists);
        let c = probe.cache.lock().unwrap();
        assert!(c.hot.contains_key("/keep-me"), "cold hit did not promote");
    }
}
