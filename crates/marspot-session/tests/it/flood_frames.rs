//! What a core is shown of a pane while its program floods it.
//!
//! The session publishes one frame per display frame while output
//! arrives a full read at a time, not one per read.  Nothing a person
//! would see may be lost to that: the screen the flood ends on is
//! published, and what a program does about a key is published as soon
//! as it is parsed.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{MsgType, encode_inject_input};

fn pane_running(script_body: &str) -> (marspot_term::test_env::StateSandbox, support::Session) {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let script = sandbox.dir.join("pane.sh");
    std::fs::write(&script, script_body).expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let session = support::Session::spawn(bin, support::COLS, support::ROWS);
    (sandbox, session)
}

fn frames(s: &support::Session) -> u64 {
    s.reader.seq() / 2
}

/// How many frames a flood gets is the gate's arithmetic and is tested
/// with it: here the machine decides how fast the program writes, and
/// a starved one's output is not a flood.
#[test]
fn a_flood_is_shown_and_ends_on_its_last_screen() {
    // sixteen megabytes of text, then one line and silence
    let (_sandbox, s) = pane_running(
        "#!/bin/sh\nyes 'the quick brown fox jumps over the lazy dog' | head -c 16000000\nprintf '\\r\\nTHE-END\\r\\n'\nexec sleep 30\n",
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while frames(&s) < 2 {
        assert!(Instant::now() < deadline, "the pane never published");
        std::thread::sleep(Duration::from_millis(1));
    }
    let from = frames(&s);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let screen = s.try_screen(support::COLS, support::ROWS).unwrap_or_default();
        if screen.iter().any(|row| row.starts_with("THE-END")) {
            break;
        }
        assert!(Instant::now() < deadline, "the flood's last screen was never published: {screen:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(frames(&s) - from >= 2, "the flood itself was never shown");
}

#[test]
fn what_a_program_does_about_a_key_is_shown_while_it_floods() {
    // one program writes the flood and, for every key it finds typed,
    // switches bracketed paste: the mode travels in the frame's header,
    // and one writer means its answer is never cut in two by the flood
    let (_sandbox, s) = pane_running(
        "#!/bin/zsh\nstty -icanon -echo min 1 time 0\nblock=''\nrepeat 60 block+=$'the quick brown fox jumps over the lazy dog\\r\\n'\non=0\nwhile :; do\n  print -rn -- $block\n  if read -t 0 -k1 k; then\n    if (( on )); then print -n '\\e[?2004l'; on=0; else print -n '\\e[?2004h'; on=1; fi\n  fi\ndone\n",
    );
    let paste_mode = |s: &support::Session| s.reader.read(&mut Vec::new(), &mut Vec::new()).map(|f| f.bracketed_paste());
    let deadline = Instant::now() + Duration::from_secs(10);
    while frames(&s) < 50 {
        assert!(Instant::now() < deadline, "the pane never flooded");
        std::thread::sleep(Duration::from_millis(5));
    }
    // a read can find the writer mid-publish every time it looks while
    // the flood runs; that frame is not an answer either way
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(mode) = paste_mode(&s) {
            break mode;
        }
        assert!(Instant::now() < deadline, "no frame could be read during the flood");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(!first, "bracketed paste was on before any key");
    for want in [true, false, true] {
        // long enough apart that no key rides on the one before it
        std::thread::sleep(Duration::from_millis(250));
        s.send_frame(MsgType::InjectInput, encode_inject_input(0, b"k"));
        let sent = Instant::now();
        while paste_mode(&s) != Some(want) {
            assert!(sent.elapsed() < Duration::from_secs(5), "a key typed into the flood was not answered in any frame");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
