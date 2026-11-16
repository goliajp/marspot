use super::*;
use crate::local_session::{LocalSession, test_pane_env};
use std::thread;
use std::time::{Duration, Instant};

fn ev(kind: PointerKind, button: u8, count: u16) -> PointerInput {
    PointerInput {
        kind,
        button,
        count,
        col: 3,
        row: 1,
        keys: b"\x1b[5~".to_vec(),
    }
}

fn modes(tracking: MouseTrackingMode, sgr: bool) -> Modes {
    Modes {
        tracking,
        sgr,
        alt_screen: false,
        app_cursor: false,
    }
}

fn bytes(e: &PointerInput, m: &Modes) -> Option<Vec<u8>> {
    let mut out = Out::new();
    encode(e, m, &mut out).then(|| out.bytes().to_vec())
}

/// The incident: a program turned on `?1000`..`?1003` and `?1006`, then
/// turned them off on its way out, and the core's copy still said on.
#[test]
fn a_report_after_the_program_turned_reporting_off_is_not_encoded() {
    use MouseTrackingMode::*;
    let on = modes(AnyEvent, true);
    assert_eq!(
        bytes(&ev(PointerKind::Motion, 3, 1), &on).unwrap(),
        b"\x1b[<35;3;1M"
    );
    let off = modes(Off, false);
    for kind in [
        PointerKind::Press,
        PointerKind::Release,
        PointerKind::Motion,
        PointerKind::Wheel,
    ] {
        assert_eq!(
            bytes(&ev(kind, 3, 1), &off),
            None,
            "{kind:?} with reporting off"
        );
    }
}

/// Each mode gets only the events it asked for.
#[test]
fn each_tracking_mode_gets_only_what_it_asked_for() {
    use MouseTrackingMode::*;
    let travel = ev(PointerKind::Motion, 3, 1);
    let drag = ev(PointerKind::Motion, 0, 1);
    let press = ev(PointerKind::Press, 0, 1);
    for (mode, travel_ok, drag_ok) in [
        (X11, false, false),
        (ButtonEvent, false, true),
        (AnyEvent, true, true),
    ] {
        let m = modes(mode, true);
        assert_eq!(asked(&travel, &m), travel_ok, "{mode:?} travel");
        assert_eq!(asked(&drag, &m), drag_ok, "{mode:?} drag");
        assert!(asked(&press, &m), "{mode:?} press");
    }
}

/// The encoding is the session's: SGR or X10, whichever is on now.
#[test]
fn the_encoding_follows_the_live_modes() {
    use MouseTrackingMode::*;
    let sgr = modes(AnyEvent, true);
    let x10 = modes(AnyEvent, false);
    assert_eq!(
        bytes(&ev(PointerKind::Release, 2, 1), &sgr).unwrap(),
        b"\x1b[<2;3;1m"
    );
    assert_eq!(
        bytes(&ev(PointerKind::Release, 2, 1), &x10).unwrap(),
        b"\x1b[M\x23\x23\x21"
    );
    assert_eq!(
        bytes(&ev(PointerKind::Motion, 3, 1), &x10).unwrap(),
        b"\x1b[M\x43\x23\x21"
    );
    assert_eq!(
        bytes(&ev(PointerKind::Wheel, 65, 2), &sgr).unwrap(),
        b"\x1b[<65;3;1M\x1b[<65;3;1M"
    );
    let far = PointerInput {
        col: 500,
        row: 4_000_000_000,
        ..ev(PointerKind::Press, 0, 1)
    };
    assert_eq!(bytes(&far, &x10).unwrap(), [0x1b, b'[', b'M', 32, 255, 255]);
    assert_eq!(bytes(&far, &sgr).unwrap(), b"\x1b[<0;500;4000000000M");
    let many = ev(PointerKind::Wheel, 64, u16::MAX);
    assert_eq!(
        bytes(&many, &sgr).unwrap().len(),
        MAX_COUNT * b"\x1b[<64;3;1M".len(),
        "capped"
    );
}

/// The wheel as arrows: only on the alt screen with no reports asked
/// for, and in the cursor-key mode on now.  After a full-screen program
/// has left the alt screen they would walk the shell's history.
#[test]
fn arrows_go_only_to_a_full_screen_program_that_asked_for_no_reports() {
    use MouseTrackingMode::*;
    let up = ev(PointerKind::Arrows, 64, 2);
    let alt = Modes {
        tracking: Off,
        sgr: false,
        alt_screen: true,
        app_cursor: false,
    };
    assert_eq!(bytes(&up, &alt).unwrap(), b"\x1b[A\x1b[A");
    let app = Modes {
        app_cursor: true,
        ..alt
    };
    assert_eq!(
        bytes(&ev(PointerKind::Arrows, 65, 1), &app).unwrap(),
        b"\x1bOB"
    );
    assert_eq!(
        bytes(
            &up,
            &Modes {
                alt_screen: false,
                ..alt
            }
        ),
        None,
        "left the alt screen"
    );
    assert_eq!(
        bytes(
            &up,
            &Modes {
                tracking: X11,
                ..alt
            }
        ),
        None,
        "wants reports instead"
    );
}

/// A held grid's modes are as stale as the core's; the tty is asked
/// only for an event the modes let through.
#[test]
fn the_gate_refuses_while_held_and_asks_the_tty_last() {
    use MouseTrackingMode::*;
    let mut g = Gate::new();
    let e = ev(PointerKind::Motion, 3, 1);
    let on = modes(AnyEvent, true);
    let mut asked_tty = false;
    assert_eq!(
        g.check(&e, true, &on, || {
            asked_tty = true;
            true
        })
        .unwrap_err(),
        Refusal::Held
    );
    assert_eq!(
        g.check(&e, false, &modes(Off, true), || {
            asked_tty = true;
            true
        })
        .unwrap_err(),
        Refusal::NotAsked
    );
    assert!(!asked_tty);
    assert_eq!(
        g.check(&e, false, &on, || false).unwrap_err(),
        Refusal::NoProgram
    );
    assert_eq!(g.check(&e, false, &on, || true).unwrap(), b"\x1b[<35;3;1M");
    let keys = ev(PointerKind::ProgramKeys, 0, 1);
    assert_eq!(
        g.check(&keys, false, &modes(Off, false), || true).unwrap(),
        b"\x1b[5~"
    );
    assert_eq!(
        g.check(&keys, false, &modes(Off, false), || false)
            .unwrap_err(),
        Refusal::NoProgram
    );
}

fn pump_until(s: &mut LocalSession, mut done: impl FnMut(&LocalSession) -> bool) -> bool {
    // a loaded runner starts a shell slowly
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        s.pump();
        if done(s) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn has_tty(s: &LocalSession, image: &[u8]) -> bool {
    program_has_the_tty(s.raw_master(), s.child_pid(), image)
}

fn front(s: &LocalSession) -> libc::pid_t {
    // SAFETY: tcgetpgrp on the master this session owns.
    unsafe { libc::tcgetpgrp(s.raw_master()) }
}

/// Waits for the answer to settle on `want`, then checks it stays.
fn settles(s: &mut LocalSession, image: &[u8], want: bool) -> bool {
    pump_until(s, |s| has_tty(s, image) == want) && {
        thread::sleep(Duration::from_millis(200));
        has_tty(s, image) == want
    }
}

/// zsh at its prompt, a program in raw mode, the same program in
/// canonical mode, and a shell that `exec`ed either.
#[test]
fn only_a_program_reading_in_raw_mode_has_the_tty() {
    let _env = test_pane_env("/bin/zsh");
    let mut s = LocalSession::spawn(0xF7, 100, 30, "", || {}).expect("spawn");
    assert!(
        pump_until(&mut s, |s| s.pty_in_raw_mode()),
        "zsh reads its line"
    );
    // the spawned image matches, and so does a shell only named in the list
    assert!(settles(&mut s, b"/bin/zsh", false), "zsh at its prompt");
    assert!(settles(&mut s, b"/not/the/shell", false), "a shell by name");

    // a byte ends it: in non-canonical mode ^D is just a byte
    s.write(b"stty -icanon -echo; head -c 1 >/dev/null; stty sane\r")
        .unwrap();
    assert!(
        settles(&mut s, b"/bin/zsh", true),
        "a program in its own group, raw"
    );
    s.write(b"x").unwrap();
    assert!(settles(&mut s, b"/bin/zsh", false), "back at the prompt");

    s.write(b"cat\r").unwrap();
    assert!(
        pump_until(&mut s, |s| front(s) != s.child_pid()),
        "cat has the foreground"
    );
    assert!(
        settles(&mut s, b"/bin/zsh", false),
        "cat in canonical mode reads lines, not reports"
    );
    s.write(b"\x04").unwrap();
    assert!(
        settles(&mut s, b"/bin/zsh", false),
        "back at the prompt again"
    );

    s.write(b"stty -icanon -echo; exec cat\r").unwrap();
    assert!(
        settles(&mut s, b"/bin/zsh", true),
        "the shell's pid now runs cat"
    );
}

/// A shell that `exec`s a shell is still the shell.
#[test]
fn a_shell_that_execs_a_shell_is_still_the_shell() {
    let _env = test_pane_env("/bin/zsh");
    let mut s = LocalSession::spawn(0xF8, 100, 30, "", || {}).expect("spawn");
    assert!(
        pump_until(&mut s, |s| s.pty_in_raw_mode()),
        "zsh reads its line"
    );
    s.write(b"exec /bin/bash --norc --noprofile\r").unwrap();
    let image = |pid: i32| {
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        String::from_utf8_lossy(&path[..n.max(0) as usize]).into_owned()
    };
    assert!(
        pump_until(&mut s, |s| image(s.child_pid()).ends_with("/bash")),
        "the pid runs bash"
    );
    assert!(
        pump_until(&mut s, |s| s.pty_in_raw_mode()),
        "bash reads its line"
    );
    // bash's own line editor in raw mode, at the shell's pid
    assert!(settles(&mut s, b"/bin/zsh", false), "bash at the zsh pid");
}
