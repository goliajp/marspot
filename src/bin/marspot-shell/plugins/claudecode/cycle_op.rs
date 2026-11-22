//! The scripts a profile switch runs, and the badge menu that offers one.

use super::*;

/// Answer a badge click that cannot be honoured, without touching the
/// pane.
///
/// Nothing is typed and nothing is held: a click the cycle refuses must
/// cost the user no more than a click that did nothing.  The badge is
/// the only channel a pane has for saying so, and riding a (very short)
/// op is what makes it appear and then put itself away — the scan loop
/// owns the badge the rest of the time and would otherwise overwrite
/// this within the second.
pub(super) fn cycle_blocked_op(badge: &'static str) -> pty_op::PtyOp {
    pty_op::PtyOp::new("cc.cycle_blocked")
        .lock_keys(false)
        .hold_screen(false)
        .escape_hatch(false)
        .badge(badge)
        .step(pty_op::Step::settle(Duration::from_secs(4)).named("show"))
}

/// Are the pane's model and effort both known?
///
/// The scan's reading and the hook's record are two reads of the same
/// model; they have to exist and agree, and the record has to name the
/// effort as well. Without all of that a pane takes no part in a switch. The id
/// carries a context suffix (`claude-fable-5-1[1m]`) that the short
/// name leaves out.
pub(super) fn confirm_choice(pane_model: Option<&str>, choice: &CliChoice) -> Result<(), &'static str> {
    let Some(pane) = pane_model else {
        return Err("the pane's model is not known");
    };
    let Some(id) = choice.model.as_deref() else {
        return Err("the hook has not recorded the model to carry");
    };
    if choice.effort.is_none() {
        return Err("the hook has not recorded the effort");
    }
    if short_model(id.split('[').next().unwrap_or(id)) != pane {
        return Err("the hook's record names a different model from the pane's");
    }
    Ok(())
}

/// The profile-cycle script: take the current claude down and bring the
/// same session back under the next profile.
///
/// SIGTERM straight to the pid rather than `/exit` through the PTY:
/// bare `exit` is ambiguous (claude treats it as a message and replies
/// politely without quitting), `/exit` works but prints `Bye!`, and both
/// echo into the grid.  A signal echoes nothing.
///
/// `claudeN` is an interactive alias in the user's rc file, so the
/// script sets the variable the alias would have set — same effect,
/// without depending on that file still defining it.
pub(super) fn profile_cycle_op(
    say: Option<&str>,
    uuid: &str,
    has_transcript: bool,
    next_profile: u8,
    claude: pidtree::ProcId,
    background_holder: Option<pidtree::ProcId>,
    shell_pid: i32,
    // where the session lives, so resuming it does not move it
    project_dir: Option<&str>,
    // the model and effort it was running with
    choice: &CliChoice,
) -> Option<pty_op::PtyOp> {
    // No uuid, no cycle: without a name for the session there is no
    // way to tell "nothing to resume" from "a conversation we failed
    // to find", and resuming *nothing* in the second case would drop
    // what the user is looking at.
    if uuid.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let mut cmd = pty_op::PtyCommand::new("claude")
        .env("CLAUDE_CONFIG_DIR", format!("{home}/.claude-profile-{next_profile}"))
        .clear_screen_first(true);
    // The claude this starts reports its own submits. This is the path
    // the badge takes, so it is the one that matters most and the one
    // that was missed when the other two were wired.
    // No submit receipt hook: nothing reads a receipt since the
    // sentence moved to the command line, and the hook forked this
    // binary on every prompt the person sent by hand.
    // A session with no transcript has taken no turn — there is
    // nothing to carry across, and `--resume` on a uuid claude never
    // wrote fails.  Start claude plain instead.
    //
    // This is the whole of the reported bug's second half: entering
    // cc and immediately clicking the badge did nothing at all, twice,
    // and went on doing nothing until the session had answered
    // something — 8.8 hours, on the pane this was found on.  The
    // window where a user is most likely to notice they are on the
    // wrong account is exactly the window where switching was refused.
    if has_transcript {
        cmd = cmd.arg("--resume").arg(uuid);
    }
    // Resumed where it lives, not where the pane's shell happens to
    // stand. `--resume` looks the id up in the project of the current
    // directory and binds what it reopens to that directory, so a
    // shell that has wandered takes the session with it: a torajs
    // session came back as `devops`, loading devops's rules, and the
    // only sign was the pane's title.
    cmd = cmd.in_dir(project_dir);
    // The model the session was on, not the new profile's default.
    // Quoted: an id like `claude-fable-5-1[1m]` is a glob to a shell.
    if let Some(model) = &choice.model {
        cmd = cmd.arg("--model").quoted_arg(model.clone());
    }
    if let Some(effort) = &choice.effort {
        cmd = cmd.arg("--effort").arg(effort.clone());
    }
    // The sentence goes in as claude's own argument, not as typing.
    //
    // It used to be pasted into the composer and followed by a
    // return, and that is the wrong path however well it works: the
    // composer is the person's buffer. Every failure leaves our text
    // sitting in it -- a machine at load 513 produced exactly that,
    // three times -- and even the successful path spends a moment
    // with marspot's words in the person's input box.
    //
    // `claude [options] [prompt]` takes it as a positional argument
    // and submits it itself. Measured 2026-10-01 through a real pty:
    // an interactive claude started this way answers the prompt
    // without the composer ever holding it, and it combines with
    // `--resume`.
    if let Some(word) = say {
        cmd = cmd.quoted_arg(word);
    }
    let line = cmd.to_bytes()?;
    // What the person had typed and not sent. Read before claude is
    // taken down, handed back once the new one is reading input.
    // Switching accounts is bookkeeping; it is not a reason to throw
    // away someone's half-written sentence.
    let held = pty_op::Job::new();
    let mut op = pty_op::PtyOp::new("cc.profile_cycle")
        .hold_screen(true)
        .badge(format!("→ P{next_profile}"))
        // Whatever the person had typed travels; the sentence this script
        // says does not. It is on the command line now and never reaches
        // the composer, but a copy an older build left there would travel.
        .step(pty_op::Step::capture_composer(Arc::clone(&held)).disowning(resume_word()))
        .step(pty_op::Step::settle(HOLD_SETTLE).named("hold_settle"))
        .step(
            pty_op::Step::terminate(claude, libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL),
        );
    // The same way the pane's own claude is taken down — a signal, and
    // gone before the resume is typed, because `claude --resume` checks
    // for a live holder the moment it starts.  Its transcript is the
    // conversation; the resume below picks it up.
    if let Some(id) = background_holder.filter(|h| h.pid != claude.pid) {
        op = op.step(
            pty_op::Step::terminate(id, libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL)
                .named("stop_background"),
        );
    }
    Some(
        // Same guard as the reclaim's: a claude that replaced the one
        // taken down would get the resume line typed into its prompt.
        op.shell_line_unless(shell_pid, line, "resume", pty_op::Step::stop_if_process(shell_pid, looks_like_claudecode))
            // Wait for the new claude to draw, exactly as the
            // reclamation does.  The hand-written version settled for a
            // flat 600 ms and unfroze onto whatever was there — fine for
            // a small session, a visible flash of shell for a big one (a
            // 317 MB transcript took 10 s+ to paint, measured
            // 2026-07-13).
            .step(
                pty_op::Step::await_process(shell_pid, looks_like_claudecode)
                    .timeout(Duration::from_secs(20)),
            )
            // A resume claude refuses exits within a second; nothing
            // after this has anything to wait for once it has gone.
            .step(pty_op::Step::watch_process(shell_pid, looks_like_claudecode))
            // Ask the terminal instead of counting its bytes.  Two
            // guesses came before this one: "quiet for a while" fired
            // on the startup trickle and the message went into a CLI
            // that was not reading input yet (2026-09-29), and "quiet
            // after 4096 bytes" was never reached by half the panes
            // (2026-09-30).  Alt screen plus bracketed paste is the
            // program itself saying it has the terminal.
            .step(pty_op::Step::await_tui_ready().or_late())
            // Then wait for it to actually paint, because the hold
            // lifts when this run ends and what is underneath is a
            // cleared screen.
            //
            // This used to be here by accident: the sentence was typed
            // in, and the submit step waits for the composer to redraw
            // with the text in it -- which is a first frame by another
            // name. Moving the sentence onto the command line took the
            // wait away with it, and the pane went black for as long as
            // the new claude took to draw (reported the same evening,
            // and the log shows `Done` nine milliseconds after
            // `await_tui_ready` gave up at 13 bytes).
            //
            // `await_tui_ready` cannot serve as the wait: it proceeds
            // on timeout by design, because a pane that will not answer
            // must not cost the resume that already happened.
            .step(
                pty_op::Step::await_quiet(idle::WAKE_QUIET_FOR)
                    .after_bytes(idle::FIRST_FRAME_BYTES)
                    .timeout(idle::WAKE_WATCHDOG)
                    // asked to carry on, it starts working at once and
                    // its spinner never lets the pane go still
                    .or_composer()
                    .named("first_frame"),
            )
            // Their sentence last. If there was one, it goes back and
            // the run ends there -- a pane with someone mid-thought in
            // it does not also need to be told the quota came back.
            .step(pty_op::Step::restore_composer(held)),
    )
}

/// Build the badge right-click menu rows: one "switch to Px" per
/// discovered profile except the pane's current one.  Tag = the
/// profile number (echoed back via `on_pane_badge_menu_action`).
/// Pure so the row shape is unit-testable without a $HOME fixture.
pub(crate) fn badge_menu_for(
    current: u8,
    profiles: &[u8],
) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
    profiles
        .iter()
        .copied()
        .filter(|&n| n != current)
        .map(|n| marspot::shell_proto::PaneBadgeMenuItem {
            tag: crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, n),
            label: strings::text(Arg::SwitchToProfile(n)),
        })
        .collect()
}
