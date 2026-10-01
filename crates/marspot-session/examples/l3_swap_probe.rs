//! Replacing a live pane's process without the pane noticing.
//!
//! This is the load-bearing mechanism of silent update. A new build has to
//! take over a running pane with the user's scrollback, their half-typed
//! command and their running program all still there -- so a replacement
//! L3 starts on the same session id, replays that session's bytelog to
//! rebuild the grid, and only once it has published does the old one get
//! killed. Promote too early and the pane goes blank mid-handover; never
//! kill the old one and the session has two writers.
//!
//! Three things are checked, in the order a swap does them:
//!   - the replacement's first published frame already carries the marker
//!     the old session printed, which is the replay working
//!   - the old process is gone after the handover, and reaped
//!   - the replacement takes input afterwards
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_swap_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};
use support::{COLS, ROWS, Session, cleanup, die, rss_kib};

const MARKER: &str = "swapmark";

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_swap_probe <marspot-session>"));

    // A session id of our own, far from anything a sandbox app allocated,
    // so the replay below reads this probe's bytelog and nobody else's.
    let id: u64 = 90_000 + (std::process::id() as u64 % 1000);

    let mut old = Session::spawn_with_id(&bin, COLS, ROWS, Some(id));
    old.wait_ready("original");
    let old_pid = old.pid();

    // Something in the history for the replay to bring back.
    old.send_line(&format!("printf '%s\\n' {MARKER}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if old
            .screen(COLS, ROWS)
            .iter()
            .any(|l| l.trim_end() == MARKER)
        {
            break;
        }
        if Instant::now() >= deadline {
            let mut all = vec![old];
            cleanup(&mut all);
            die(format!("{MARKER:?} never reached the original session's screen"));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // Give the bytelog time to have the marker on disk: the replay reads
    // the file, not the grid.
    std::thread::sleep(Duration::from_millis(500));

    // Stage the replacement on the same session id, as `begin_swap` does.
    let new = Session::spawn_with_id(&bin, COLS, ROWS, Some(id));
    let staged = Instant::now() + Duration::from_secs(10);
    while new.reader.seq() == 0 {
        if Instant::now() >= staged {
            let mut all = vec![old, new];
            cleanup(&mut all);
            die("the replacement never published, so there was nothing to promote");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The first frame is the one that matters: promotion happens on it, so
    // if the history is not in it the pane blinks empty.
    let replayed = new.screen(COLS, ROWS);
    let carried = replayed.iter().any(|l| l.contains(MARKER));

    // Promote: the old one goes only now.
    old.kill();
    std::thread::sleep(Duration::from_millis(300));
    let old_still_there = rss_kib(old_pid).is_some();

    // And the replacement takes input.
    let (c0, r0) = new.cursor();
    let before = new.reader.seq();
    new.send_char('s');
    new.wait_seq_past(before, "replacement echo after promotion");
    std::thread::sleep(Duration::from_millis(150));
    let landed = new.cell(c0, r0, COLS);
    let new_pid = new.pid();

    let mut all = vec![new];
    cleanup(&mut all);

    if !carried {
        die(format!(
            "the replacement published without the history: wanted {MARKER:?}, \
             first rows were {:?}",
            &replayed[..replayed.len().min(3)]
        ));
    }
    if old_still_there {
        die(format!("the original {old_pid} is still resident after the handover"));
    }
    if landed != 's' {
        die(format!(
            "the replacement does not take input: ({c0},{r0}) shows {landed:?}"
        ));
    }
    println!(
        "PASS: session {id} handed from {old_pid} to {new_pid} -- replay carried {MARKER:?}, \
         the old one is gone, the new one echoes"
    );
}
