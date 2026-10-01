//! One session under continuous output: does its memory plateau?
//!
//! The commitment is that nothing gets heavier the longer it runs, and
//! the per-session engine is where the candidates live -- PTY chunks,
//! the terminal and grid, the scrollback. The product multiplies this by
//! the pane count, so a slow leak here is a leak times nine.
//!
//! A plateau, not a ceiling: RSS climbs while the scrollback ring and the
//! allocator reach their working size, and that is not a leak. So the
//! samples are split in four and the last quarter is compared with the
//! first. A leak keeps climbing and fails the ratio; a plateau does not.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_drift_probe -- \
//!       target/release/marspot-session 300 10

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};
use support::{COLS, ROWS, Session, cleanup, die, rss_kib};

/// Last quarter over first quarter.  1.10 leaves room for the ring and
/// the allocator settling while a steady climb of even a few MiB a minute
/// over a five-minute window lands far above it.
const DRIFT_MAX: f64 = 1.10;

fn main() {
    let mut args = std::env::args().skip(1);
    let bin: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_drift_probe <marspot-session> <duration_s> <interval_s>"));
    let duration: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let interval: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let drift_max: f64 = std::env::var("L3_DRIFT_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DRIFT_MAX);
    let want = (duration / interval).floor() as usize;
    if want < 8 {
        die(format!(
            "{want} samples cannot be split into four quarters that mean anything; \
             raise DURATION_S or lower INTERVAL_S"
        ));
    }

    let session = Session::spawn(&bin, COLS, ROWS);
    session.wait_ready("session");
    let pid = session.pid();

    // Continuous output from the shell's own side, so the bytes travel the
    // path a real pane's do: PTY -> parser -> grid -> scrollback -> shm.
    session.send_line("while :; do seq 1 2000; done");
    std::thread::sleep(Duration::from_millis(800));
    let moved = session.reader.seq();
    std::thread::sleep(Duration::from_millis(400));
    if session.reader.seq() == moved {
        let mut all = vec![session];
        cleanup(&mut all);
        die("the session is not publishing, so nothing is being driven -- \
             the load command did not take");
    }

    let mut samples: Vec<u64> = Vec::with_capacity(want);
    let t0 = Instant::now();
    while samples.len() < want {
        std::thread::sleep(Duration::from_secs_f64(interval));
        match rss_kib(pid) {
            Some(kib) => samples.push(kib),
            None => {
                die(format!("session {pid} died {:.0}s in", t0.elapsed().as_secs_f64()));
            }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    // Still publishing at the end: a session that stopped producing output
    // halfway would plateau for the best possible reason and the worst.
    let last_seq = session.reader.seq();
    std::thread::sleep(Duration::from_millis(300));
    let still_moving = session.reader.seq() != last_seq;

    let mut all = vec![session];
    cleanup(&mut all);

    if !still_moving {
        die("the session stopped publishing before the window ended -- \
             a plateau measured after the load died is not a plateau");
    }

    let q = samples.len() / 4;
    let mean = |s: &[u64]| s.iter().sum::<u64>() as f64 / s.len() as f64;
    let q1 = mean(&samples[..q]);
    let q4 = mean(&samples[samples.len() - q..]);
    let ratio = q4 / q1;
    println!(
        "{} samples over {elapsed:.0}s: q1 {q1:.0} KiB, q4 {q4:.0} KiB, drift {ratio:.3}x \
         (min {} max {})",
        samples.len(),
        samples.iter().min().unwrap(),
        samples.iter().max().unwrap()
    );
    if ratio > drift_max {
        die(format!(
            "RSS drift {ratio:.3}x over {drift_max:.2}x -- it is still climbing at the end \
             of the window, which is what a leak looks like"
        ));
    }
    println!("PASS: drift {ratio:.3}x ≤ {drift_max:.2}x under {elapsed:.0}s of continuous output");
}
