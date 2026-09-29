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
    let mut pty = spawn("stty -echo; cat > /dev/null");
    let mut t = Terminal::new(60, 10);
    pump(&mut pty, &mut t, Duration::from_millis(400));

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

#[test]
fn a_program_that_echoes_keeps_its_local_echo() {
    // The other side of the same coin: `cat` echoes, so the guesses
    // are confirmed and nothing is taken back.
    let mut pty = spawn("stty -echo -icanon; cat");
    let mut t = Terminal::new(60, 10);
    pump(&mut pty, &mut t, Duration::from_millis(400));

    for &b in b"hello" {
        assert!(t.predict_byte(b));
        let _ = pty.write(&[b]);
        std::thread::sleep(Duration::from_millis(15));
        pump(&mut pty, &mut t, Duration::from_millis(20));
    }
    let settle = t.predict_deadline() + Duration::from_millis(150);
    pump(&mut pty, &mut t, settle);

    assert_eq!(row0(&t), "hello");
    assert_eq!(t.predictions_expired, 0, "nothing had to be taken back");
    assert!(t.predictions_hit >= 5);
}
