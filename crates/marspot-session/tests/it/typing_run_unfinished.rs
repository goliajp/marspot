//! A run whose status line the shell never ran -- the command was
//! interrupted, or an error dropped the rest of the paste -- ends at
//! the end the shell's prompt hook prints, once the prompt is quiet.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{PaneWatchFired, TypedResult, WatchKind, run_trailer};
use marspot_term::test_env::{StateSandbox, sandbox_state_dir};

use crate::pane_link::{Link, body, wait};
use crate::support::{COLS, ROWS, Session};
use crate::typing_run_end::{run, run_end, shell, slow_zsh, swap};

/// A run ends within this once its shell is back at the prompt: the
/// second of quiet, another when the shell was seen back late, and what
/// a loaded host adds.
const SOON: Duration = Duration::from_millis(4500);

fn on_screen(s: &Session, line: &str) -> bool {
    s.try_screen(COLS, ROWS).is_some_and(|rows| rows.iter().any(|r| r.starts_with(line)))
}

/// zsh at its prompt, with hooks that print a command end with the
/// status before every prompt and a start before every command.
fn hooked_zsh(sandbox: &mut StateSandbox) -> Session {
    let s = slow_zsh(sandbox);
    wait(15.0, "the prompt", || on_screen(&s, "READY>"));
    s
}

/// bash 3.2 whose hook prints an end with the status before every
/// prompt, after `setup` in its rc file.
fn hooked_bash(sandbox: &mut StateSandbox, setup: &str) -> Session {
    assert!(std::path::Path::new("/bin/bash").exists(), "/bin/bash is missing");
    sandbox.env.set("BASH_SILENCE_DEPRECATION_WARNING", "1");
    let rc = sandbox.dir.join("bashrc");
    std::fs::write(&rc, format!("PS1='READY> '\nPROMPT_COMMAND='s=$?; printf \"\\033]133;D;%s\\007\" $s'\n{setup}\n"))
        .expect("write the rc file");
    shell(sandbox, &format!("/bin/bash --noprofile --rcfile '{}'", rc.display()))
}

/// Interrupt a run once its command is under way: what it reports, and
/// how long after that.
fn interrupted(s: &Session) -> (PaneWatchFired, Duration) {
    let mut l = Link::new(s);
    let input = format!("echo under-way; sleep 20\n{}", run_trailer(false));
    let req = l.type_in(input.as_bytes(), true, true, true, Some(run_end(1, 0)));
    assert_eq!(l.typed(req, 10.0).result, TypedResult::Written);
    wait(10.0, "the command", || on_screen(s, "under-way"));
    assert!(l.try_fired(1, 1.5).is_none(), "the command is running");
    let at = Instant::now();
    l.inject("\x03");
    let f = l.fired(1, 15.0);
    (f, at.elapsed())
}

#[test]
fn typing_a_run_that_is_interrupted_ends_with_130_in_zsh() {
    let mut sandbox = sandbox_state_dir();
    let s = hooked_zsh(&mut sandbox);
    let (f, took) = interrupted(&s);
    assert_eq!(f.status, Some(130));
    assert!(took < SOON, "{took:?}");
}

#[test]
fn typing_a_run_that_is_interrupted_ends_with_130_in_bash() {
    let mut sandbox = sandbox_state_dir();
    let s = hooked_bash(&mut sandbox, "");
    wait(10.0, "bash's prompt", || on_screen(&s, "READY>"));
    let (f, took) = interrupted(&s);
    assert_eq!(f.status, Some(130));
    assert!(took < SOON, "{took:?}");
}

/// zsh drops the rest of a paste after an error.
#[test]
fn typing_a_run_that_is_an_error_to_zsh_ends_with_1() {
    let mut sandbox = sandbox_state_dir();
    let s = hooked_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    for (id, command) in [(1, "echo )"), (2, "echo ${unset?no}")] {
        let at = Instant::now();
        let (f, screen) = run(&s, &mut l, id, command);
        assert_eq!(f.status, Some(1), "{command}\n{screen:#?}");
        assert!(at.elapsed() < SOON, "{command}: {:?}", at.elapsed());
    }
    let (f, _) = run(&s, &mut l, 3, "(exit 7)");
    assert_eq!(f.status, Some(7), "the next run is ended by its own line");
}

/// The end a shell prints as it starts is not the run's while a builtin
/// runs in silence, with the shell itself in front: its hook said that
/// a command started.
#[test]
fn typing_a_run_of_a_silent_builtin_into_a_shell_still_starting_returns_its_status() {
    let mut sandbox = sandbox_state_dir();
    let s = slow_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let at = Instant::now();
    let (f, screen) = run(&s, &mut l, 1, "zselect -t 300; (exit 6)");
    assert_eq!(f.status, Some(6), "{screen:#?}");
    assert!(at.elapsed() > Duration::from_secs(3), "{:?}", at.elapsed());
}

/// The same for a program in front, from a shell whose hooks do not
/// say that a command starts.
#[test]
fn typing_a_run_of_a_silent_program_into_bash_still_starting_returns_its_status() {
    let mut sandbox = sandbox_state_dir();
    let s = hooked_bash(&mut sandbox, "read -t 1 _ < <(sleep 2)");
    let mut l = Link::new(&s);
    let (f, screen) = run(&s, &mut l, 1, "sh -c 'sleep 4; exit 9'");
    assert_eq!(f.status, Some(9), "{screen:#?}");
}

/// A hook's end from before an image swap ends a run that is asked
/// about again after it.
#[test]
fn typing_a_run_that_was_interrupted_before_an_image_swap_ends_after_it() {
    let mut sandbox = crate::self_execv::Sandbox::new();
    let s = crate::self_execv::start(&mut sandbox);
    let mut l = Link::new(&s);
    let p0 = l.pos();
    l.inject("printf '\\033]133;D;%s\\007' 130\r");
    l.armed(body(1, WatchKind::CommandEnd, p0));
    let end = l.fired(1, 10.0);
    let mut l = swap(&sandbox, &s, l);
    l.armed(run_end(4, p0));
    let f = l.fired(4, 5.0);
    assert_eq!((f.status, f.pos), (Some(130), end.pos));
}
