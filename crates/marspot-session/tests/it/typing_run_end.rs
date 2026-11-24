//! A run is ended by its own status line alone: not by the command end
//! a hook of the shell prints, at its start or before the status line.

use std::os::unix::fs::PermissionsExt;

use marspot_term::shell_proto::{MsgType, PaneWatchFired, RUN_END_MARK, TypedResult, WatchBody, WatchKind, WatchReason, run_trailer};
use marspot_term::test_env::{StateSandbox, sandbox_state_dir};

use crate::pane_link::{Link, bin, body, wait, with_script, with_zsh};
use crate::support::{COLS, ROWS, Session};

/// A watch for a run's end, as `pane.run` asks for it.
pub(crate) fn run_end(watch_id: u32, since_pos: u64) -> WatchBody {
    WatchBody { text: RUN_END_MARK.into(), ..body(watch_id, WatchKind::CommandEnd, since_pos) }
}

/// Run `command` as `pane.run` does: what the watch reports, and the
/// screen as it was published then, read once as `pane.run` reads it.
pub(crate) fn run(s: &Session, l: &mut Link, watch_id: u32, command: &str) -> (PaneWatchFired, Vec<String>) {
    let input = format!("{command}\n{}", run_trailer(false));
    let req = l.type_in(input.as_bytes(), true, true, true, Some(run_end(watch_id, 0)));
    let f = l.fired(watch_id, 20.0);
    let screen = s.try_screen(COLS, ROWS).expect("a frame is published when the watch is reported");
    assert_eq!(l.typed(req, 1.0).result, TypedResult::Written);
    l.cancel(watch_id);
    (f, screen)
}

/// A shell script that execs `command`, as the pane's shell.
pub(crate) fn shell(sandbox: &mut StateSandbox, command: &str) -> Session {
    let script = sandbox.dir.join("shell");
    std::fs::write(&script, format!("#!/bin/sh\nexec {command}\n")).expect("write the shell script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);
    Session::spawn(bin(), COLS, ROWS)
}

/// zsh with a slow rc file that leaves the status 1, and a prompt hook
/// that prints a command end with the status before every prompt.
pub(crate) fn slow_zsh(sandbox: &mut StateSandbox) -> Session {
    let dir = sandbox.dir.path().to_path_buf();
    std::fs::write(
        dir.join(".zshrc"),
        "PS1='READY> '\n\
         started=\n\
         precmd() { local s=$?; [[ -z $started ]] && s=1; started=1; printf '\\033]133;D;%s\\007\\033]133;A\\007' $s; }\n\
         preexec() { printf '\\033]133;C\\007'; }\n\
         zmodload zsh/zselect; zselect -t 150\n",
    )
    .expect("write .zshrc");
    shell(sandbox, &format!("/usr/bin/env ZDOTDIR='{}' /bin/zsh -d", dir.display()))
}

#[test]
fn typing_a_run_into_a_shell_still_starting_returns_the_commands_status() {
    for (command, status, output) in [
        ("echo hi; true", 0, Some("hi")),
        ("(exit 7)", 7, None),
        ("true", 0, None),
        ("printf 'a\\nb\\n'; (exit 3)", 3, Some("b")),
    ] {
        let mut sandbox = sandbox_state_dir();
        let s = slow_zsh(&mut sandbox);
        let mut l = Link::new(&s);
        let (f, screen) = run(&s, &mut l, 1, command);
        assert_eq!(f.status, Some(status), "{command}\n{screen:#?}");
        if let Some(o) = output {
            assert!(screen.iter().any(|r| r == o), "{command}: {o:?} is not on the screen\n{screen:#?}");
        }
    }
}

/// Without bracketed paste the command runs, and the prompt hook's end
/// comes, before the status line does.
#[test]
fn typing_a_run_into_bash_whose_prompt_hook_prints_an_end_first_returns_the_commands_status() {
    assert!(std::path::Path::new("/bin/bash").exists(), "/bin/bash is missing");
    let mut sandbox = sandbox_state_dir();
    sandbox.env.set("BASH_SILENCE_DEPRECATION_WARNING", "1");
    let rc = sandbox.dir.join("bashrc");
    std::fs::write(&rc, "PS1='READY> '\nPROMPT_COMMAND=\"printf '\\033]133;D;5\\007'\"\n").expect("write the rc file");
    let s = shell(&mut sandbox, &format!("/bin/bash --noprofile --rcfile '{}'", rc.display()));
    wait(10.0, "bash's prompt", || s.try_screen(COLS, ROWS).is_some_and(|r| r.iter().any(|r| r.starts_with("READY>"))));
    let mut l = Link::new(&s);
    for (id, command, status) in [(1, "sleep 0.3; (exit 4)", 4), (2, "true", 0), (3, "sleep 0.3; true", 0)] {
        let (f, screen) = run(&s, &mut l, id, command);
        assert_eq!(f.status, Some(status), "{command}\n{screen:#?}");
    }
}

/// A caller that asks for any end, or a shell-side build from before
/// the mark, still gets the hook's.
#[test]
fn typing_run_end_a_plain_command_end_watch_takes_an_unmarked_end() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.armed(body(1, WatchKind::CommandEnd, p));
    l.armed(run_end(2, p));
    l.inject("printf '\\033]133;D;%s\\007' 5\r");
    assert_eq!(l.fired(1, 10.0).status, Some(5));
    assert!(l.try_fired(2, 0.3).is_none(), "not a run's end");
    l.inject("printf '\\033]133;D;%s;marspot=run\\007' 6\r");
    assert_eq!(l.fired(2, 10.0).status, Some(6));
}

#[test]
fn typing_run_end_is_found_behind_other_ends_of_its_batch() {
    let mut sandbox = sandbox_state_dir();
    let s = with_script(
        &mut sandbox,
        "printf READY; read _; printf '\\033]133;D;1\\007\\033]133;D;7;marspot=run\\007'; \
         for i in 1 2 3 4 5 6 7 8 9 10 11 12; do printf '\\033]133;D;2\\007'; done; printf DONE; exec sleep 600",
    );
    let mut l = Link::new(&s);
    let p = l.pos();
    l.inject("\r");
    crate::pane_link::screen_shows(&s, "DONE");
    // registered after the fact, as after a restart of the core
    l.armed(run_end(1, p));
    assert_eq!(l.fired(1, 5.0).status, Some(7));
}

#[test]
fn typing_run_end_survives_an_image_swap_behind_another_end() {
    let mut sandbox = crate::self_execv::Sandbox::new();
    let s = crate::self_execv::start(&mut sandbox);
    let mut l = Link::new(&s);
    let p0 = l.pos();
    l.inject("printf '\\033]133;D;%s;marspot=run\\007' 3\r");
    l.armed(run_end(1, p0));
    let first = l.fired(1, 10.0);
    l.inject("printf '\\033]133;D;%s\\007' 9\r");
    l.armed(body(2, WatchKind::CommandEnd, first.pos));
    assert_eq!(l.fired(2, 10.0).status, Some(9));
    let mut l = swap(&sandbox, &s, l);
    l.armed(run_end(4, p0));
    let f = l.fired(4, 2.0);
    assert_eq!((f.status, f.pos), (Some(3), first.pos), "the run's end, not the later one");
    l.armed(body(5, WatchKind::CommandEnd, p0));
    assert_eq!(l.fired(5, 2.0).status, Some(9));
}

/// An image that knows the mark and saw none hands over that it saw
/// none: a hook's end before the swap stands for no run's.
#[test]
fn typing_run_end_is_not_a_hooks_end_from_before_an_image_swap() {
    let mut sandbox = crate::self_execv::Sandbox::new();
    let s = crate::self_execv::start(&mut sandbox);
    let mut l = Link::new(&s);
    let p0 = l.pos();
    l.inject("printf '\\033]133;D;%s\\007' 9\r");
    l.armed(body(1, WatchKind::CommandEnd, p0));
    assert_eq!(l.fired(1, 10.0).status, Some(9));
    let mut l = swap(&sandbox, &s, l);
    l.armed(run_end(4, p0));
    assert!(l.try_fired(4, 0.3).is_none(), "an unmarked end before the swap");
    l.inject("printf '\\033]133;D;%s;marspot=run\\007' 6\r");
    assert_eq!(l.fired(4, 10.0).status, Some(6));
}

pub(crate) fn swap(sandbox: &crate::self_execv::Sandbox, s: &Session, mut l: Link) -> Link {
    l.armed(run_end(3, u64::MAX - 1));
    crate::self_execv::stage(sandbox.path(), true);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    assert_eq!(l.fired(3, 30.0).reason, WatchReason::Restarting);
    let pid = s.pid() as i32;
    wait(30.0, "the new image", || {
        crate::self_execv::count_in_log(sandbox.path(), "L3_EXECV_RESUMED") >= 1 && unsafe { libc::kill(pid, 0) } == 0
    });
    let socket = marspot_term::session_registry::session_socket_path(crate::self_execv::id());
    Link::on(crate::support::reattach(&socket, 15).expect("reattach after the swap"))
}

/// An end parsed in a turn that took in a full read is reported once
/// the frame that shows the output before it is out, though the frames
/// of a flood are spaced out.
#[test]
fn typing_run_end_closing_a_flood_is_reported_with_its_last_line_on_screen() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let flood = sandbox.dir.join("flood");
    for id in 1..=10 {
        let mut out = "flood line\n".repeat(40_000 + id * 400);
        out.push_str(&format!("last-{id}\n\x1b]133;D;0;marspot=run\x07"));
        std::fs::write(&flood, out).expect("write the flood");
        let p = l.pos();
        l.armed(run_end(id as u32, p));
        l.inject(&format!("cat '{}'\r", flood.display()));
        l.fired(id as u32, 20.0);
        let screen = s.try_screen(COLS, ROWS).expect("a frame is published when the watch is reported");
        l.cancel(id as u32);
        assert!(screen.iter().any(|r| r.starts_with(&format!("last-{id}"))), "run {id}\n{screen:#?}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
