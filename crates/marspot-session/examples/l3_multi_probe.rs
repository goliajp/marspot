//! N sessions at once, each echoing only its own character.
//!
//! What this catches is a session collision: two panes that end up
//! sharing a session id, a PTY, or a shm region. The symptom in the
//! product is one pane showing another pane's output, and the way to
//! provoke it is to start several at once and give each a distinct
//! character to echo. A crossed pair shows up as the wrong character,
//! not as a crash.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01 against the
//! contract `bin/soak-l3.sh` still had for it -- argv is the session
//! binary and N, exit 0 is a pass, and no session may outlive the probe.
//!
//!     cargo run -p marspot-session --example l3_multi_probe -- \
//!       target/release/marspot-session 4

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use support::{COLS, ROWS, Session, cleanup, die};

fn main() {
    let mut args = std::env::args().skip(1);
    let bin: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_multi_probe <marspot-session> <n>"));
    let n: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| die("second argument must be the session count"));
    if n < 2 {
        die("fewer than two sessions cannot collide, so this would prove nothing");
    }
    // Distinct, and all of them printable so a crossed pair is legible in
    // the failure message rather than an escape code.
    let chars: Vec<char> = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();

    let mut sessions: Vec<Session> = (0..n).map(|_| Session::spawn(&bin, COLS, ROWS)).collect();
    for (i, s) in sessions.iter().enumerate() {
        s.wait_ready(&format!("session #{i}"));
    }

    // Every pid distinct is the cheapest collision check there is, and it
    // runs before anything is typed.
    let mut pids: Vec<u32> = sessions.iter().map(|s| s.pid()).collect();
    let before = pids.len();
    pids.sort_unstable();
    pids.dedup();
    if pids.len() != before {
        cleanup(&mut sessions);
        die("two sessions report the same pid");
    }

    // Where each one's cursor sits before it is typed into: the echo has
    // to land there, and the prompt length differs per shell.
    let anchors: Vec<(u16, u16)> = sessions.iter().map(|s| s.cursor()).collect();
    let seqs: Vec<u64> = sessions.iter().map(|s| s.reader.seq()).collect();

    for (i, s) in sessions.iter().enumerate() {
        s.send_char(chars[i]);
    }
    for (i, s) in sessions.iter().enumerate() {
        s.wait_seq_past(seqs[i], &format!("echo on session #{i}"));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));

    let mut wrong: Vec<String> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        let (c0, r0) = anchors[i];
        let landed = s.cell(c0, r0, COLS);
        if landed != chars[i] {
            wrong.push(format!(
                "#{i} typed {:?} but ({c0},{r0}) shows {landed:?}",
                chars[i]
            ));
        }
    }

    cleanup(&mut sessions);

    if !wrong.is_empty() {
        die(format!(
            "{} of {n} sessions echoed the wrong character -- {}",
            wrong.len(),
            wrong.join("; ")
        ));
    }
    println!("PASS: {n} sessions, distinct pids, each echoed only its own character");
}
