use super::boot_assembly_tests::Sandbox;
use super::*;
use marspot_term::session_registry as reg;

/// 2026-07-28 incident — a session whose registry entry is gone
/// must exit on its own.  L3s outlive their L2/L1 by design, so
/// the entry is the only ownership record; every reaper works by
/// removing it.  Before the deadman, a test script (or anything)
/// that deleted the registry out from under live sessions created
/// processes NOTHING could ever find or kill — 176 of them helped
/// push the machine into a forced reboot.
#[test]
fn a_session_whose_registry_entry_is_deleted_exits_by_itself() {
    let mut sb = Sandbox::new();
    // The deadman's beat is five seconds in production; what this
    // asserts is that it fires at all, not how often.
    sb.set("MARSPOT_SESSION_ENTRY_CHECK_MS", "200");
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let live = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    let pid = live.session().l3_pid().expect("spawned L3 has a pid");
    // once its entry is gone the registry no longer names it, so
    // the sandbox is told about it to kill it if the deadman fails
    sb.own(pid);
    std::mem::forget(live);
    assert!(reg::pid_is_live_session(pid));

    std::fs::remove_file(reg::session_entry_path(sid)).expect("remove entry");

    // Generous: the beat above is 200 ms, and the margin is for a
    // loaded box, not for the cadence.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if !reg::pid_is_live_session(pid) {
            return; // exited on its own — the leak class is closed
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("session {pid} outlived its deleted registry entry");
}

/// The whole `PaneResetMouseReporting` chain, against a real L3.
///
/// Every piece of this was unit-tested separately — the codec, the
/// terminal's reset, the pty_op step that sends it — and the thing
/// those cannot tell you is whether the frame reaches the terminal
/// that matters.  What went wrong on 2026-09-01 lived between the
/// layers, not inside one.
///
/// Drives a shell in a real session process into mouse tracking,
/// checks the mirror L2 renders from agrees, then sends the frame
/// and waits for the mirror to say it is off.
#[test]
fn a_real_session_gives_up_mouse_reporting_when_told() {
    // Short tag on purpose: it lands in the sandbox path, and the
    // L3's socket under it has to fit macOS's 104-byte `sun_path`.
    // `mouse-reset` overran it by a few bytes and the session died
    // at `uds_bind_failed` before it could register.
    let _sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");

    // The shell has to *print* the mode set for the terminal to
    // parse it: modes are set by the program's output, and
    // anything injected here is its input.  Retried because the
    // first bytes land before the shell is reading.
    //
    // Only the first copy to run prints anything.  A shell that
    // takes longer to start than one retry has every copy queued,
    // and the ones still waiting when the reset below arrives
    // would turn the mode back on behind it: under a full suite's
    // load this failed that way once, twenty seconds in, where a
    // run alone takes a quarter of a second.
    let once = std::env::temp_dir().join(format!("marspot-mouse-once-{}-{sid}", std::process::id()));
    let _ = std::fs::remove_file(&once);
    let command = format!(
        "[ -e '{0}' ] || {{ : > '{0}'; printf '\\033[?1002h\\033[?1006h'; }}\r",
        once.display()
    );
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut on = false;
    while std::time::Instant::now() < deadline {
        pane.session_mut().forward_inject_input(command.as_bytes());
        for _ in 0..40 {
            pane.pump();
            if pane.session().l3_mouse_tracking_active() {
                on = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if on {
            break;
        }
    }
    assert!(on, "shell never got as far as turning mouse tracking on");
    assert!(
        pane.session().l3_mouse_sgr_active(),
        "1006 rides with 1002; without it the wheel encodes the old way"
    );

    // What L1 sends once it has taken a pane's foreground program
    // down.  Nothing here kills anything — the point is that the
    // terminal drops the mode on the frame alone, because a signal
    // gives it nothing else to go on.
    pane.session_mut().forward_pane_reset_mouse_reporting();

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        pane.pump();
        if !pane.session().l3_mouse_tracking_active() {
            assert!(
                !pane.session().l3_mouse_sgr_active(),
                "SGR encoding must go with it, or the next wheel is \
                 still a mouse report — just in the older form"
            );
            let _ = std::fs::remove_file(&once);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("L3 kept reporting mouse tracking after being told to stop");
}

/// A pane's shell is told which terminal it is talking to.
///
/// TERM used to be replaced only when it looked broken, so a pane
/// launched from a session that exported some other terminal's
/// name handed that on, and programs addressed a terminal that is
/// not here.  This sets a wrong one deliberately and checks the
/// shell does not see it.
#[test]
fn a_pane_is_not_given_the_launchers_idea_of_the_terminal() {
    let mut sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();

    sb.set("TERM", "screen-256color");
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx).expect("real L3 spawn (is marspot-session built?)");

    pane.session_mut().forward_inject_input(b"printf 'TERM=[%s]\\n' \"$TERM\"\r");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        pane.pump();
        let screen = pane.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n");
        if screen.contains("TERM=[xterm-256color]") {
            assert!(
                !screen.contains("TERM=[screen-256color]"),
                "the launcher's TERM reached the shell:\n{screen}"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the shell never reported its TERM:\n{}", pane.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n"));
}

/// A new pane opens where the shell actually is.
///
/// The registry records where a session was launched; every pane
/// that has since `cd`'d somewhere would open in the wrong place
/// if that were the answer.  This drives a real shell into a
/// directory it was not started in and asks.
#[test]
fn a_new_pane_inherits_the_directory_the_shell_is_in_now() {
    let _sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");

    let launched = live_cwd_of(sid);
    assert!(!launched.is_empty(), "the shell has to have a directory to begin with");

    // Somewhere it certainly was not started in.
    pane.session_mut().forward_inject_input(b"cd /usr/lib\r");

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        pane.pump();
        let now = live_cwd_of(sid);
        if now.ends_with("/usr/lib") {
            assert_ne!(now, launched, "the answer is the live cwd, not the launch one");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the shell never reported /usr/lib; still {:?}", live_cwd_of(sid));
}

/// `?1002` and `?1003` arrive as different answers.
///
/// Press and release are reported by every tracking mode, so they
/// needed no knowledge of which was on.  Motion is the half that
/// differs, and a program that asked for drag-only and is sent
/// every idle pointer move gets buried.  The shm carries two more
/// bits for it; this checks they mean what they say against a real
/// shell setting each mode.
#[test]
fn the_two_motion_modes_are_told_apart_on_a_real_pty() {
    for (mode, want_drag, want_any) in
        [("1000", false, false), ("1002", true, false), ("1003", true, true)]
    {
        let _sb = Sandbox::new();
        let (tx, _rx) = mpsc::channel();
        let sid = reg::allocate_next_session_id().unwrap();
        let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut on = false;
        while std::time::Instant::now() < deadline && !on {
            pane.session_mut()
                .forward_inject_input(format!("printf '\\033[?{mode}h'\r").as_bytes());
            for _ in 0..40 {
                pane.pump();
                if pane.session().l3_mouse_tracking_active() {
                    on = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(on, "?{mode} never reached the terminal");

        assert_eq!(
            pane.session().l3_mouse_motion_wanted(true),
            want_drag,
            "?{mode}: motion while a button is held"
        );
        assert_eq!(
            pane.session().l3_mouse_motion_wanted(false),
            want_any,
            "?{mode}: motion with no button"
        );
    }
}

/// A pasteboard only this test process names, selected for the
/// product's copy and paste, and handed back to the pasteboard server
/// when dropped: a named pasteboard otherwise lasts until logout.
struct OwnPasteboard(String);

impl OwnPasteboard {
    fn new(sb: &mut Sandbox) -> Self {
        let name = format!("marspot-test-{}", std::process::id());
        sb.set(marspot::input::PASTEBOARD_ENV, &name);
        Self(name)
    }
}

impl Drop for OwnPasteboard {
    fn drop(&mut self) {
        let pb = objc2_app_kit::NSPasteboard::pasteboardWithName(
            &objc2_foundation::NSString::from_str(&self.0),
        );
        let _: () = unsafe { objc2::msg_send![&*pb, releaseGlobally] };
    }
}

/// OSC 52 travels from the program to the Mac's pasteboard.
///
/// Three processes are involved and only the last one can touch a
/// pasteboard: the program writes the escape, L3 parses it and
/// has no AppKit, L2 receives the frame and writes it.  The unit
/// tests cover the parse; this covers the journey, by putting a
/// value on a real pasteboard through a real shell and reading
/// it back.  The pasteboard is the test's own: the general one
/// belongs to whoever is at the machine and to every other run on it.
#[test]
fn osc52_reaches_the_pasteboard_from_a_real_shell() {
    let mut sb = Sandbox::new();
    let _pb = OwnPasteboard::new(&mut sb);
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");

    let token = format!("marspot-osc52-{}", std::process::id());
    let b64 = {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let src = token.as_bytes();
        let mut out = String::new();
        for c in src.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= c.len() {
                    out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    };
    // The encoder above is the test's own; the decoder under test
    // is the one in the terminal.  Check they agree before relying
    // on it, or a broken encoder would read as a broken journey.
    assert_eq!(
        marspot_term::base64::decode(b64.as_bytes(), 4096).unwrap(),
        token.as_bytes(),
        "the test's encoder and the terminal's decoder disagree"
    );

    assert_eq!(
        marspot::input::read_clipboard_text(),
        None,
        "a pasteboard nobody has written to holds nothing"
    );

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut sent = false;
    while std::time::Instant::now() < deadline {
        if !sent {
            pane.session_mut().forward_inject_input(
                format!("printf '\\033]52;c;{b64}\\007'\r").as_bytes(),
            );
            sent = true;
        }
        pane.pump();
        if marspot::input::read_clipboard_text().as_deref() == Some(token.as_str()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the clipboard never got {token:?}");
}

/// A click reaches the program, on a real PTY.
///
/// The encoding is unit-tested; this is the other half — that a
/// button press forwarded through `l3_inject_mouse_button` comes
/// out of the pty as the bytes a program reads.  `cat -v` makes
/// them visible, so the assertion is on what the program itself
/// received rather than on what we believe we sent.
#[test]
fn a_click_reaches_the_program_on_a_real_pty() {
    let _sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");

    // Turn tracking on the same way the other test does — the
    // shell has to print the mode for the terminal to see it.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut on = false;
    while std::time::Instant::now() < deadline && !on {
        pane.session_mut()
            .forward_inject_input(b"printf '\\033[?1002h\\033[?1006h'\r");
        for _ in 0..40 {
            pane.pump();
            if pane.session().l3_mouse_tracking_active() {
                on = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(on, "shell never got as far as turning mouse tracking on");

    // Read what the program receives, with the escapes shown.
    pane.session_mut().forward_inject_input(b"cat -v\r");
    std::thread::sleep(std::time::Duration::from_millis(400));
    for _ in 0..20 {
        pane.pump();
    }

    pane.session_mut().l3_inject_mouse_button(0, true, 12, 3);
    pane.session_mut().l3_inject_mouse_button(0, false, 12, 3);

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        pane.pump();
        let screen = pane.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n");
        if screen.contains("[<0;12;3M") && screen.contains("[<0;12;3m") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let screen = pane.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n");
    panic!("the press and release never reached the program; screen was:\n{screen}");
}
