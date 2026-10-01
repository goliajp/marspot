//! A local-echo guess is settled by the bytes that come back.  Some
//! programs send none: a `sudo` password prompt turns echo off and
//! prints nothing at all until Enter.  Against a real pty running a
//! program shaped that way, the guesses have to come back off the
//! screen on their own — otherwise what is on screen is the password.
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use marspot_term::terminal::Terminal;
use std::time::{Duration, Instant};

/// Read whatever the pty has and feed it, for `dur`.
fn pump(pty: &mut Pty, t: &mut Terminal, dur: Duration) {
    let start = Instant::now();
    let mut buf = [0u8; 8192];
    while start.elapsed() < dur {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => t.feed(&buf[..n]),
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
        t.expire_predictions();
    }
}

fn row0(t: &Terminal) -> String {
    (0..t.grid().cols())
        .map(|c| t.grid().cell(c, 0).ch)
        .collect::<String>()
        .replace('\0', " ")
        .trim_end()
        .to_string()
}

/// Pump until the program says it is up, then take its marker back
/// off the screen.
///
/// These tests used to give the shell 400 ms and start typing.  That
/// is a guess about how fast a machine starts `/bin/sh` and applies
/// `stty`, and on a loaded one it is wrong — the first `predict_byte`
/// lands before echo is off and the test fails about the scheduler
/// rather than about local echo.  The program itself says when it is
/// ready; `stty` has already run by the time it can.
fn wait_until_ready(pty: &mut Pty, t: &mut Terminal) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => t.feed(&buf[..n]),
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
        t.expire_predictions();
        if row0(t).contains("R3ADY") {
            // The marker is the program's own output, so it is on the
            // screen; clear it here rather than asking the program to,
            // which would be a second thing to wait for.
            t.feed(b"\x1b[2J\x1b[H");
            return;
        }
    }
    panic!("the program never announced itself; row0 was {:?}", row0(t));
}

fn spawn(program: &str) -> Pty {
    let pty = Pty::spawn(PtyConfig {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), program.into()],
        size: TerminalSize {
            cols: 60,
            rows: 10,
            ..Default::default()
        },
        env_remove_prefixes: vec!["MARSPOT_".into()],
            env_set: Vec::new(),
        ..Default::default()
    })
    .expect("spawn");
    unsafe {
        let fd = pty.raw_master();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    pty
}

#[test]
fn a_password_prompt_does_not_keep_what_was_typed_on_screen() {
    // Echo off and not a byte sent back — the shape of `sudo`'s
    // password prompt, without needing a real one.
    let mut pty = spawn("stty -echo; printf R3ADY; cat > /dev/null");
    let mut t = Terminal::new(60, 10);
    wait_until_ready(&mut pty, &mut t);

    let secret = b"hunter22";
    for &b in secret {
        assert!(t.predict_byte(b), "the pane is still predicting");
        let _ = pty.write(&[b]);
        std::thread::sleep(Duration::from_millis(15));
        t.expire_predictions();
    }
    // Mid-typing the guesses are on screen — that is local echo doing
    // its job, and is exactly why it must not be permanent.
    let settle = t.predict_deadline() + Duration::from_millis(150);
    pump(&mut pty, &mut t, settle);

    assert_eq!(row0(&t), "", "the typed password is off the screen");
    assert!(!t.predictions_pending());
    assert!(t.predictions_expired > 0, "expiry is what cleared it");
}

/// Feed until the program has echoed `want` bytes in total.
///
/// `seen` counts every byte read since the caller started counting, so
/// the wait is for the thing itself rather than for a duration that
/// happens to be long enough on an idle machine.
fn pump_until_echoed(
    pty: &mut Pty,
    t: &mut Terminal,
    seen: &mut usize,
    want: usize,
    limit: Duration,
) {
    let start = Instant::now();
    let mut buf = [0u8; 8192];
    while *seen < want && start.elapsed() < limit {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => {
                *seen += n;
                t.feed(&buf[..n]);
            }
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
        t.expire_predictions();
    }
}

#[test]
fn a_program_that_echoes_keeps_its_local_echo() {
    // The other side of the same coin: `cat` echoes, so the guesses
    // are confirmed and nothing is taken back.
    let mut pty = spawn("stty -echo -icanon; printf R3ADY; cat");
    let mut t = Terminal::new(60, 10);
    wait_until_ready(&mut pty, &mut t);

    // Each byte is guessed, sent, and then waited for -- waited for by
    // counting what came back, not by sleeping.  What this test is
    // about is the order of a guess and its confirmation, and a sleep
    // only settles that order on an idle machine: on a loaded one the
    // echo of `h` can arrive after `e` has been guessed, and the row
    // reads `lhelo`.  That is the scheduler being observed, not local
    // echo being wrong (seen three times on 2026-10-01, on a host at
    // load 83-97, and on an unmodified tree).
    let mut echoed = 0usize;
    for (i, &b) in b"hello".iter().enumerate() {
        assert!(t.predict_byte(b));
        let _ = pty.write(&[b]);
        pump_until_echoed(&mut pty, &mut t, &mut echoed, i + 1, Duration::from_secs(10));
    }
    assert_eq!(echoed, 5, "the fixture needs every byte echoed back");
    let settle = t.predict_deadline() + Duration::from_millis(150);
    pump(&mut pty, &mut t, settle);

    assert_eq!(row0(&t), "hello");
    assert_eq!(t.predictions_expired, 0, "nothing had to be taken back");
    assert!(t.predictions_hit >= 5);
}
