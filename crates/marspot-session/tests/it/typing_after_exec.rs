//! A shell that turned bracketed paste on hands the pane to one that
//! never does: the next command typed must not reach it in paste
//! markers, even when it is typed right after the `exec`.

use marspot_term::shell_proto::{TypedResult, WatchKind};
use marspot_term::test_env::sandbox_state_dir;

use crate::pane_link::{Link, body, with_zsh};

/// Type `command` and its command-end line as one input with a
/// command-end watch; the status the watch reports.
fn run(l: &mut Link, watch_id: u32, command: &str) -> Option<i16> {
    let input = format!("{command}\nprintf '\\033]133;D;%s\\007' $?");
    let req = l.type_in(input.as_bytes(), true, true, true, Some(body(watch_id, WatchKind::CommandEnd, 0)));
    let f = l.fired(watch_id, 10.0);
    assert_eq!(l.typed(req, 1.0).result, TypedResult::Written);
    f.status
}

/// zsh turns the mode off as it takes the `exec` line, but the command
/// is typed before zsh has read that line.  Without waiting for it the
/// paste went out in markers in most runs; ten panes make a
/// pass by luck unlikely.
#[test]
fn typing_right_after_exec_into_bash_without_bracketed_paste() {
    assert!(std::path::Path::new("/bin/bash").exists(), "/bin/bash is missing");
    for i in 0..10 {
        let mut sandbox = sandbox_state_dir();
        sandbox.env.set("BASH_SILENCE_DEPRECATION_WARNING", "1");
        let s = with_zsh(&mut sandbox);
        let mut l = Link::new(&s);
        let req = l.type_in(b"exec /bin/bash --norc --noprofile", true, true, true, None);
        assert_eq!(l.typed(req, 5.0).result, TypedResult::Written);
        assert_eq!(run(&mut l, 1, "false"), Some(1), "pane {i}: the first command after the exec");
        assert_eq!(run(&mut l, 2, "true"), Some(0), "pane {i}: the second");
    }
}

/// `pane.read` asks for the output position with a request that types
/// nothing; one between the `exec` and the command must not make the
/// command forget the return it has to wait for.
#[test]
fn typing_after_exec_with_a_paste_probe_in_between() {
    assert!(std::path::Path::new("/bin/bash").exists(), "/bin/bash is missing");
    for i in 0..10 {
        let mut sandbox = sandbox_state_dir();
        sandbox.env.set("BASH_SILENCE_DEPRECATION_WARNING", "1");
        let s = with_zsh(&mut sandbox);
        let mut l = Link::new(&s);
        let req = l.type_in(b"exec /bin/bash --norc --noprofile", true, true, true, None);
        assert_eq!(l.typed(req, 5.0).result, TypedResult::Written);
        let probe = l.type_in(b"", false, false, false, None);
        assert_eq!(l.typed(probe, 1.0).result, TypedResult::Written, "pane {i}: the probe");
        assert_eq!(run(&mut l, 1, "false"), Some(1), "pane {i}: the first command after the exec");
    }
}
