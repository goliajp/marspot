//! Keystroke to the character appearing, across the IPC hop L3 added.
//!
//! Splitting the pane into its own process put a socket and a shared
//! region between a key and the pixels it changes. This measures a key
//! written on the control socket until that character is in the published
//! grid, and reports the distribution rather than an average, because the
//! complaint a user makes is about the slow ones.
//!
//! **This is the predicted echo, not the shell's.** L3 puts the character
//! on screen itself rather than waiting for the PTY to echo it back
//! (`predict_expiry`), which is why the numbers are tens of microseconds
//! and not the milliseconds a PTY round trip costs. That is the right
//! thing to measure -- it is when the user sees the character -- but it is
//! not a measurement of the round trip, and reading it as one would make
//! the IPC hop look free.
//!
//! It fails only on a gross regression. The shape it exists to catch is
//! an event loop that polls instead of waiting: that puts a floor under
//! every keystroke at the poll interval, which shows up as a p50 moving
//! into the tens of milliseconds while the mean still looks fine.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_latency_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};
use support::{COLS, ROWS, Session, cleanup, die};

/// Keys measured.  Enough for a p99 to mean something, few enough that
/// they fit on one row of an 80-column grid without wrapping.
const KEYS: usize = 60;

/// p99 ceiling in milliseconds.  Measured on the dev box 2026-10-01, two
/// runs: p50 0.02 ms, p99 0.05-0.06 ms, max 0.77-0.85 ms over 60 keys.
///
/// 5 ms is ~80x that p99, and below the floor a 10 ms poll interval would
/// put under every keystroke -- which is the point. The first draft said
/// 25 ms, which a polling loop would have passed, so the ceiling would
/// have tolerated exactly the regression it names.
const P99_CEIL_MS: f64 = 5.0;

/// Above this the host is not quiet enough for the ceiling to mean
/// anything, and a breach is reported as undecided rather than as a
/// regression.
const LOAD_CEIL: f64 = 2.0;

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_latency_probe <marspot-session>"));
    let ceil_ms: f64 = std::env::var("L3_LATENCY_P99_CEIL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(P99_CEIL_MS);

    let session = Session::spawn(&bin, COLS, ROWS);
    session.wait_ready("session");

    let mut samples: Vec<f64> = Vec::with_capacity(KEYS);
    let mut lost = 0usize;
    // Where the echo has to appear.  Waiting on the publish sequence
    // alone measures the wrong thing: a session publishes for its own
    // reasons, so the first version of this reported a p50 of 10
    // microseconds -- which is not a round trip through a socket, a PTY,
    // a shell's echo and a shared region, it is the next unrelated
    // publish arriving. The character landing where the cursor was is
    // the only condition this keystroke caused.
    let (mut col, row) = session.cursor();
    for i in 0..KEYS {
        let c = (b'a' + (i % 26) as u8) as char;
        let t0 = Instant::now();
        session.send_char(c);
        let deadline = t0 + Duration::from_secs(2);
        loop {
            if session.cell(col, row, COLS) == c {
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
                col += 1;
                break;
            }
            if Instant::now() >= deadline {
                lost += 1;
                break;
            }
            // Yield rather than sleep: a 1 ms sleep quantises every sample
            // to the sleep and reports the sleep back as the latency.  But
            // not a bare spin either -- reading the whole grid at full
            // speed is itself load, and on a busy box it delayed the
            // session it was timing badly enough to hang the run.
            std::thread::yield_now();
        }
        if col >= COLS {
            die(format!("ran out of row at key {i}; KEYS must fit one row"));
        }
    }

    // Read after the measurement, not before: a one-minute average taken
    // first describes the minute before the probe ran.
    let load = unsafe {
        let mut avg = [0f64; 3];
        if libc::getloadavg(avg.as_mut_ptr(), 3) > 0 {
            avg[0]
        } else {
            -1.0
        }
    };

    let mut all = vec![session];
    cleanup(&mut all);

    if lost > 0 {
        die(format!("{lost} of {KEYS} keystrokes never produced a publish"));
    }
    if samples.len() < KEYS {
        die(format!("only {} of {KEYS} samples", samples.len()));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    let (p50, p99, max) = (pick(0.50), pick(0.99), samples[samples.len() - 1]);

    println!(
        "keystroke→publish over {KEYS} keys: p50 {p50:.2} ms, p99 {p99:.2} ms, \
         max {max:.2} ms (host load1 {load:.2})"
    );
    if p99 > ceil_ms {
        // A ceiling derived from a quiet machine fails on a busy one, and
        // calling that a regression is how a threshold teaches people to
        // relock it rather than look.  Four of these probes run back to
        // back in `soak-l3.sh`, which on a dev box is enough: p99 went
        // 0.05 ms idle to 5.45 ms at load 9.  So a breach under load is
        // reported as undecided, with its own exit code, the way the
        // bench gate separates FAILED from INCONCLUSIVE.
        if load > LOAD_CEIL {
            eprintln!(
                "INCONCLUSIVE: p99 {p99:.2} ms over {ceil_ms:.0} ms, but host load1 was \
                 {load:.2} (> {LOAD_CEIL:.1}).  Re-measure on an idle host."
            );
            std::process::exit(75);
        }
        die(format!(
            "p99 {p99:.2} ms over the {ceil_ms:.0} ms ceiling at load {load:.2} -- an event \
             loop that polls rather than waits puts a floor under every keystroke"
        ));
    }
    println!("PASS: p99 {p99:.2} ms ≤ {ceil_ms:.0} ms at load {load:.2}");
}
