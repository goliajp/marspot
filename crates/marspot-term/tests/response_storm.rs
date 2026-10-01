//! The brake for a capability-response echo loop only pulls on a loop.
//!
//! 2026-06-15: every pane filled with literal `[?62;1;6;22c` -- DA1
//! answers fed back into the pty by a stuck shell loop. The brake was
//! added so the log could name the pane instead of leaving the symptom
//! (cell contents) to point at nothing.
//!
//! It then warned on every agent restart instead. A program starting
//! up asks the same question several times in a row -- claudecode
//! sends five `CSI ? u` inside 100 ms while it pushes keyboard flags
//! -- and one log had 54 warnings, every one of them a TUI booting.
//! A real loop among those would have gone unread.
//!
//! A loop does not stop. That is the difference these tests hold.

use marspot_term::terminal::Terminal;

/// `CSI ? u` -- the kitty keyboard query, which is what was bursting.
const QUERY: &[u8] = b"\x1b[?u";

fn warned(t: &Terminal) -> bool {
    t.response_burst_warned()
}

#[test]
fn a_program_starting_up_is_not_a_loop() {
    let mut t = Terminal::new(40, 8);
    // Five in a row, as fast as a program can ask, then it gets on
    // with its life.
    for _ in 0..5 {
        t.feed(QUERY);
        let _ = t.take_response();
    }
    assert!(!warned(&t), "a burst that stops is a program booting");
}

#[test]
fn asking_forever_is_a_loop() {
    let mut t = Terminal::new(40, 8);
    // Keep asking until it is noticed, rather than for a length of
    // time chosen to be long enough.
    //
    // No sleeping. The window is 100 ms wide and wants five in it, and
    // a sleep asked for in milliseconds is a floor, not a promise: the
    // first version of this slept 5 ms between single queries, which
    // is twenty to a window on an idle machine and four on a busy one
    // -- it passed here and failed on CI. Sleeping less does not fix
    // it, it only makes the overshoot that empties the window rarer.
    //
    // Feeding without pause leaves the window full at every instant --
    // unless this thread is taken off the CPU for longer than the
    // window, which empties it and restarts the one-second streak the
    // detector is counting. Waiting a fixed second and a half then
    // reads a stall as "no storm": that is what it did on a host at
    // load 121. So the loop ends when the thing it is waiting for has
    // happened, with a deadline far past any stall rather than at the
    // edge of one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !warned(&t) && std::time::Instant::now() < deadline {
        t.feed(QUERY);
        let _ = t.take_response();
    }
    assert!(warned(&t), "a storm that does not stop is what this is for");
}

/// The gap is what tells them apart, so a program that boots, works,
/// and boots again must not accumulate into a warning.
#[test]
fn bursts_with_quiet_between_them_do_not_add_up() {
    let mut t = Terminal::new(40, 8);
    for _ in 0..4 {
        for _ in 0..5 {
            t.feed(QUERY);
            let _ = t.take_response();
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(!warned(&t), "four separate restarts are still four restarts");
}
