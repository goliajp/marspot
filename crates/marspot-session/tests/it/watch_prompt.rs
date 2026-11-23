//! `prompt` watches: when a shell's prompt counts as drawn.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{WatchBody, WatchKind, WatchReason};
use marspot_term::test_env::sandbox_state_dir;

use crate::pane_link::{Link, body, with_zsh};
use crate::watch::end_mark;

/// Give the zsh a `precmd` that runs `slow`, silent, and a prompt with
/// or without the OSC 133 marks around it, and wait until it is drawn.
fn slow_precmd(l: &mut Link, marks: bool, slow: &str) {
    let ps1 = if marks { r"$'%{\e]133;A\a%}slow> %{\e]133;B\a%}'" } else { "'slow> '" };
    let p = l.pos();
    l.inject(&format!("precmd() {{ {slow} }}; PS1={ps1}\r"));
    l.armed(WatchBody { quiet_ms: 900, ..body(20, WatchKind::Quiet, p) });
    l.fired(20, 10.0);
    l.cancel(20);
}

/// In a shell that marks where input starts, the prompt watch after a
/// command holds once the prompt is drawn, not in the quiet while
/// `precmd` runs.
#[test]
fn watch_prompt_waits_for_the_input_start_mark_of_a_shell_that_marks_it() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    slow_precmd(&mut l, true, "sleep 0.3");
    for i in 0..3 {
        let p = l.pos();
        l.armed(body(1, WatchKind::Prompt, p));
        let started = Instant::now();
        l.inject("true\r");
        let f = l.fired(1, 5.0);
        let held = started.elapsed();
        assert!(held >= Duration::from_millis(300), "run {i}: held {held:?} after the command");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(f.pos, l.pos(), "run {i}: output came after the watch held");
        l.cancel(1);
    }
}

/// A command that is silent longer than the unmarked quiet leaves none
/// of it to the `precmd` after it: the quiet is measured from when the
/// shell was back in front.
#[test]
fn watch_prompt_after_a_silent_command_waits_for_a_slow_precmds_prompt() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    // a precmd that runs a program, and one the shell itself sleeps in
    for slow in ["sleep 0.5", "zmodload zsh/zselect; zselect -t 50"] {
        slow_precmd(&mut l, true, slow);
        // past the quiet when the command ends, and not quite
        for (command, least) in [("sleep 1.5", 2000), ("sleep 0.8", 1300)] {
            let p = l.pos();
            l.armed(body(1, WatchKind::Prompt, p));
            let started = Instant::now();
            l.inject(&format!("{command}\r"));
            let f = l.fired(1, 8.0);
            let held = started.elapsed();
            assert!(held >= Duration::from_millis(least), "{slow} after {command}: held {held:?} after the command");
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(f.pos, l.pos(), "{slow} after {command}: output came after the watch held");
            l.cancel(1);
        }
    }
}

/// The same shell without the marks: the watch is taken at its quiet,
/// as before.
#[test]
fn watch_prompt_in_a_shell_without_input_start_marks_holds_at_quiet() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    slow_precmd(&mut l, false, "sleep 0.3");
    let p = l.pos();
    l.armed(body(1, WatchKind::Prompt, p));
    l.inject("true\r");
    let f = l.fired(1, 5.0);
    assert!(f.pos > p && f.reason == WatchReason::Met, "{f:?}");
}

/// A zsh that marks input starts, then `switch`, after which the pane's
/// shell sets no such mark: the first prompt watch after the switch
/// waits for the mark until the shell has been quiet for a second, and
/// from then on the pane is taken at its quiet alone.  `end` is what
/// each later command line adds: a command end the shell prints, or
/// nothing.
fn prompt_after_the_shell_stops_marking(switch: &str, end: &str) {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.inject("PS1=$'%{\\e]133;A\\a%}marked> %{\\e]133;B\\a%}'\r");
    l.armed(body(1, WatchKind::Prompt, p));
    l.fired(1, 5.0);
    l.cancel(1);

    let p = l.pos();
    l.armed(body(2, WatchKind::Prompt, p));
    let started = Instant::now();
    l.inject(switch);
    let f = l.fired(2, 5.0);
    let held = started.elapsed();
    assert!(f.pos > p && f.reason == WatchReason::Met, "watch 1: {f:?}");
    assert!(held >= Duration::from_secs(1) && held < Duration::from_secs(2), "watch 1: held {held:?}");
    l.cancel(2);

    for i in 0..2 {
        let p = l.pos();
        l.armed(body(3, WatchKind::Prompt, p));
        let started = Instant::now();
        l.inject(&format!("true\r{end}"));
        let f = l.fired(3, 5.0);
        let held = started.elapsed();
        assert!(f.pos > p && f.reason == WatchReason::Met, "watch {}: {f:?}", i + 2);
        assert!(held >= Duration::from_millis(150) && held < Duration::from_secs(2), "watch {}: held {held:?}", i + 2);
        l.cancel(3);
    }
}

/// The zsh's prompt loses the mark and the shell still prints command
/// ends.
#[test]
fn watch_prompt_after_a_shell_stops_marking_input_starts_holds_at_quiet() {
    prompt_after_the_shell_stops_marking(&format!("PS1='plain> '\r{}", end_mark("$?")), &end_mark("$?"));
}

/// The zsh is replaced in place by a shell that prints neither marks nor
/// command ends, as `exec bash` does.
#[test]
fn watch_prompt_after_exec_into_a_shell_without_marks_holds_at_quiet() {
    prompt_after_the_shell_stops_marking("exec env PS1='plain> ' /bin/sh\r", "");
}
