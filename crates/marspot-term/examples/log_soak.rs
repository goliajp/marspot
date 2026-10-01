//! Emit N structured log events as fast as the sink will take them.
//!
//! What this is for: `bin/soak-log-rotate.sh` checks that logx's rotation
//! and its startup GC keep the log directory bounded while several
//! processes write to it at once. It used to drive that with
//! `marspot-shelld --log-soak`, and RFC-003 deleted that daemon in June --
//! after which the soak ran a binary left in `target/` and reported green
//! for a layer the product no longer has.
//!
//! The thing under test is logx, which lives in this crate, so the driver
//! belongs here too. One process per worker, N events each, in-process:
//! a flag on a shipping binary would be a product surface added for a
//! test, and a process per event would be 160 000 spawns.
//!
//!     MARSPOT_STATE_DIR=<dir> \
//!       cargo run -p marspot-term --example log_soak -- <n> [tag]

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("usage: log_soak <n> [tag]");
            std::process::exit(2);
        });
    let tag = args.next().unwrap_or_else(|| "SOAK".to_string());

    marspot_term::logx::init("soak");
    // Each line has to be big enough that N of them cross the rotation
    // cap; a terse line would make the soak pass by never rotating.
    let payload: String = std::iter::repeat_n('x', 160).collect();
    for i in 0..n {
        marspot_term::lx_event!(
            "LOG_SOAK",
            "a line written to make the sink rotate",
            tag = tag,
            seq = i,
            payload = payload
        );
    }
    // Rotation hands compression and retention to a detached thread, so a
    // process that exits the moment it stops writing takes its own prune
    // with it.  A long-lived pane never has that problem; a soak worker
    // that restarts in a loop has it on every pass, and the leftover
    // backups look like the retention rule failing.  Wait for it.
    //
    // `MARSPOT_LOG_SETTLE_MS=0` turns the wait off, which is how the
    // difference between "the rule does not hold" and "the worker raced
    // its own thread" was established.
    let settle: u64 = std::env::var("MARSPOT_LOG_SETTLE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    if settle > 0 {
        std::thread::sleep(std::time::Duration::from_millis(settle));
    }
    // Say how many, so a worker that died early is visible in the soak's
    // output rather than showing up only as a smaller directory.
    println!("wrote {n} events under {tag}");
}
