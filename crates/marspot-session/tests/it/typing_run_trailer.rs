//! A run's command-end line is always typed: the status is the
//! command's in a shell whose prompt hooks print command ends, after
//! exec into a shell without them, and in shells without hooks.

use std::os::unix::fs::PermissionsExt;

use marspot_term::shell_proto::{TypedResult, WatchKind, run_trailer};
use marspot_term::test_env::{StateSandbox, sandbox_state_dir};

use crate::pane_link::{Link, bin, body, wait, with_zsh};
use crate::support::{COLS, ROWS, Session};

/// The command and the command-end line, typed as `pane.run` types
/// them; the status the watch reports.
fn run(l: &mut Link, watch_id: u32, command: &str) -> Option<i16> {
    let input = format!("{command}\n{}", run_trailer(false));
    let req = l.type_in(input.as_bytes(), true, true, true, Some(body(watch_id, WatchKind::CommandEnd, 0)));
    let f = l.fired(watch_id, 10.0);
    assert_eq!(l.typed(req, 1.0).result, TypedResult::Written);
    l.cancel(watch_id);
    f.status
}

/// A pane with zsh whose prompt hooks print `OSC 133` marks the way
/// terminal shell integrations do: a command end with its status before
/// every prompt, an output start before every command.
fn with_hooked_zsh(sandbox: &mut StateSandbox) -> Session {
    let dir = sandbox.dir.path();
    std::fs::write(
        dir.join(".zshrc"),
        "PS1='READY> '\n\
         precmd() { local s=$?; printf '\\033]133;D;%s\\007\\033]133;A\\007' $s; }\n\
         preexec() { printf '\\033]133;C\\007'; }\n",
    )
    .expect("write .zshrc");
    let script = dir.join("hooked-zsh");
    std::fs::write(&script, format!("#!/bin/sh\nZDOTDIR='{}' exec /bin/zsh -d\n", dir.display())).expect("write the shell script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);
    let s = Session::spawn(bin(), COLS, ROWS);
    wait(10.0, "the prompt", || prompts(&s) >= 1);
    s
}

fn screen(s: &Session) -> Vec<String> {
    s.try_screen(COLS, ROWS).unwrap_or_default()
}

fn prompts(s: &Session) -> usize {
    screen(s).iter().filter(|r| r.starts_with("READY>")).count()
}

#[test]
fn typing_a_run_into_zsh_with_prompt_hooks_returns_the_status() {
    let mut sandbox = sandbox_state_dir();
    let s = with_hooked_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    assert_eq!(run(&mut l, 1, "echo hi"), Some(0));
    wait(10.0, "the output", || screen(&s).iter().any(|r| r == "hi"));
    assert_eq!(run(&mut l, 2, "false"), Some(1));
    assert_eq!(run(&mut l, 3, "(exit 3)"), Some(3));
    // the line is typed whatever the shell prints
    wait(10.0, "the status line", || screen(&s).iter().any(|r| r.contains("marspot=run")));
}

/// A hooked zsh that hands the pane to a shell without hooks: no hook
/// prints an end any more, and the typed line still gives the status.
#[test]
fn typing_a_run_after_exec_from_a_hooked_zsh_into_a_plain_one_still_gets_the_status() {
    let mut sandbox = sandbox_state_dir();
    let s = with_hooked_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let req = l.type_in(b"exec /bin/zsh -f", true, true, true, None);
    assert_eq!(l.typed(req, 5.0).result, TypedResult::Written);
    assert_eq!(run(&mut l, 1, "false"), Some(1));
    assert_eq!(run(&mut l, 2, "true"), Some(0));
}

#[test]
fn typing_a_run_into_zsh_without_hooks_returns_the_status() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    assert_eq!(run(&mut l, 1, "false"), Some(1));
    assert_eq!(run(&mut l, 2, "echo hi"), Some(0));
    // nothing prints an end for the shell, so the line had to be typed
    wait(10.0, "the status line", || screen(&s).iter().any(|r| r.contains("marspot=run")));
}

/// bash 3.2 never turns bracketed paste on: the two lines go in as typed.
#[test]
fn typing_a_run_into_bash_without_hooks_returns_the_status() {
    assert!(std::path::Path::new("/bin/bash").exists(), "/bin/bash is missing");
    let mut sandbox = sandbox_state_dir();
    sandbox.env.set("BASH_SILENCE_DEPRECATION_WARNING", "1");
    let shell = marspot_term::test_env::shell_without_rc_files(sandbox.dir.path(), "/bin/bash");
    sandbox.env.set("SHELL", &shell);
    let s = Session::spawn(bin(), COLS, ROWS);
    wait(10.0, "bash's prompt", || s.try_screen_and_cursor(COLS, ROWS).is_some_and(|(_, (c, _))| c > 0));
    let mut l = Link::new(&s);
    assert_eq!(run(&mut l, 1, "(exit 4)"), Some(4));
    assert_eq!(run(&mut l, 2, "true"), Some(0));
}
