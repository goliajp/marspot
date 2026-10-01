//! One session killed; the other one keeps working.
//!
//! This is what the process boundary is for. Before L3, a parser panic or
//! a PTY fault took the whole window with it; with a process per pane the
//! blast radius is one pane. The claim has two halves and both are
//! checked here: the sibling still echoes, and the dead session's shm
//! region is still readable -- because L2 holds a reader on it and a
//! region that faults on read would take L2 down, which is the very thing
//! the split was supposed to prevent.
//!
//! SIGKILL rather than a panic on purpose: it is the harshest exit, with
//! no unwinding and no chance to tidy up, so anything that survives it
//! survives the gentler ones.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_crash_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;
use support::{COLS, ROWS, Session, cleanup, die};

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_crash_probe <marspot-session>"));

    let victim = Session::spawn(&bin, COLS, ROWS);
    let survivor = Session::spawn(&bin, COLS, ROWS);
    victim.wait_ready("victim");
    survivor.wait_ready("survivor");
    let victim_pid = victim.pid();
    let survivor_pid = survivor.pid();

    // Both were alive and publishing before anything was killed, or the
    // test proves nothing about isolation.
    if victim.reader.seq() == 0 || survivor.reader.seq() == 0 {
        die("a session was not publishing before the kill");
    }
    let victim_seq = victim.reader.seq();

    // SIGKILL, not Child::kill's politeness.
    unsafe {
        if libc::kill(victim_pid as i32, libc::SIGKILL) != 0 {
            die(format!(
                "could not SIGKILL {victim_pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    // Reap it so the sample below is of a dead process, not a zombie that
    // still answers.
    let mut victim = victim;
    let _ = victim.child.wait();
    std::thread::sleep(Duration::from_millis(300));
    if support::rss_kib(victim_pid).is_some() {
        die(format!("{victim_pid} still has an RSS after SIGKILL"));
    }

    // Half one: the dead session's region still reads. If this faults,
    // the probe dies by signal rather than by assertion -- which is
    // itself the finding, and the exit code says so.
    let dead_seq = victim.reader.seq();
    let _ = victim.cell(0, 0, COLS);
    if dead_seq != victim_seq {
        die(format!(
            "the dead session's region changed after it died ({victim_seq} -> {dead_seq})"
        ));
    }

    // Half two: the survivor still echoes.
    let (c0, r0) = survivor.cursor();
    let seq_before = survivor.reader.seq();
    survivor.send_char('k');
    survivor.wait_seq_past(seq_before, "survivor echo after sibling died");
    std::thread::sleep(Duration::from_millis(150));
    let landed = survivor.cell(c0, r0, COLS);

    let mut all = vec![victim, survivor];
    cleanup(&mut all);

    if landed != 'k' {
        die(format!(
            "survivor {survivor_pid} stopped echoing after its sibling was killed: \
             ({c0},{r0}) shows {landed:?}"
        ));
    }
    println!(
        "PASS: {victim_pid} SIGKILLed; its region still reads at seq {victim_seq}, \
         and {survivor_pid} echoed on"
    );
}
