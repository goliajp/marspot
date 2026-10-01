//! Scrolling into history, and back, with the window following.
//!
//! The pane publishes a window onto a grid it owns, so a wheel event has
//! to move that window: L2 sends `Scroll`, L3 moves its view offset and
//! publishes the rows at the new position. If the offset is ignored, the
//! wheel does nothing; if it is applied but not published, the screen
//! freezes while the session thinks it scrolled. Both look the same from
//! outside, and both are a pane that will not show you what just scrolled
//! past.
//!
//! The check is content, not an offset field: build numbered history,
//! scroll up, assert the visible rows changed, scroll back, assert they
//! return. A session that reports an offset it did not render passes the
//! first half and fails this.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_scroll_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use marspot_term::shell_proto::{MsgType, encode_grid_scroll};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use support::{COLS, ROWS, Session, cleanup, die};

/// Wait until the screen differs from `from`, or give up.
fn wait_changed(s: &Session, from: &[String], what: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = s.screen(COLS, ROWS);
        if now != from {
            return now;
        }
        if Instant::now() >= deadline {
            die(format!("{what}: the published window did not change"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_scroll_probe <marspot-session>"));

    let session = Session::spawn(&bin, COLS, ROWS);
    session.wait_ready("session");

    // Numbered lines, so a window at the wrong offset is legible in the
    // failure rather than just "different".
    eprintln!("[scroll] session ready, sending load");
    session.send_line("seq 1 400");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if session.screen(COLS, ROWS).iter().any(|l| l.trim() == "400") {
            break;
        }
        if Instant::now() >= deadline {
            let mut all = vec![session];
            cleanup(&mut all);
            die("the history never finished printing");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let live = session.screen(COLS, ROWS);
    eprintln!("[scroll] history printed; first row {:?} last {:?}", live.first(), live.last());

    // Up into history by a whole screen: an absolute offset in rows,
    // counted back from the live bottom.
    session.send_frame(MsgType::GridScroll, encode_grid_scroll(ROWS));
    eprintln!("[scroll] sent GridScroll into history");
    let scrolled = wait_changed(&session, &live, "scroll back into history");
    eprintln!("[scroll] window moved; first row {:?}", scrolled.first());

    // And back down to live, which is offset zero.
    session.send_frame(MsgType::GridScroll, encode_grid_scroll(0));
    let returned = wait_changed(&session, &scrolled, "scroll forward to live");

    let mut all = vec![session];
    cleanup(&mut all);

    if returned != live {
        die(format!(
            "scrolled back to live but the window differs; first row was {:?}, is now {:?}",
            live.first(),
            returned.first()
        ));
    }
    println!(
        "PASS: scrolled into history (first row {:?} -> {:?}) and back to live",
        live.first().map(|s| s.as_str()).unwrap_or(""),
        scrolled.first().map(|s| s.as_str()).unwrap_or("")
    );
}
