//! The two actions that need asking about go through one place.
//!
//! S4-03 §3.3 says a plugin writing bytes into a pane's pty, and a plugin
//! signalling a process, are decided per call and not per install: "this
//! plugin may type" cannot be judged at install time, and "the line it is
//! about to type is `claude --resume <uuid>`" can. So both have to pass a
//! checkpoint, and the checkpoint goes where the op is enqueued -- one
//! point, not one per step.
//!
//! That design rests on there being no second way in. These tests are that
//! premise, asserted rather than assumed: today both actions reach the
//! outside world through `pty_op`'s runner and nothing else, and a new
//! bypass would make a checkpoint there a thing people trust and route
//! around. The tiers and the dialog §3.3 describes are a product decision
//! and are not here; this is what has to be true before either can be.
//!
//! Source shape, not behaviour. A test that drove a real plugin could not
//! see the path not taken, which is exactly what is being checked.

use std::path::Path;

const PLUGINS: &str = "src/bin/marspot-shell/plugins";

/// Every `.rs` under the plugins tree, with its path.
fn plugin_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).expect("the plugins tree is readable") {
            let p = e.expect("a readable entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).expect("a readable source file");
                out.push((p.to_string_lossy().into_owned(), text));
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new(PLUGINS), &mut out);
    assert!(
        out.len() > 5,
        "found {} plugin sources, so this test is looking in the wrong place",
        out.len()
    );
    out
}

/// Lines outside a trailing `#[cfg(test)]` block, numbered from 1.
///
/// Test code is allowed to do both of these things -- a spy host exists to
/// receive them -- so including it would make the check fail for the one
/// reason that does not matter.
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let cut = text
        .lines()
        .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
        .unwrap_or(usize::MAX);
    text.lines()
        .enumerate()
        .take_while(|(i, _)| *i < cut)
        .map(|(i, l)| (i + 1, l))
        .collect()
}

/// A signal is sent in exactly one place, and that place is the op runner.
///
/// `libc::kill(pid, 0)` is not a signal -- it asks whether a pid exists --
/// so it is allowed anywhere.
#[test]
fn signalling_happens_only_inside_the_op_runner() {
    let mut offenders = Vec::new();
    for (path, text) in plugin_sources() {
        for (n, line) in production_lines(&text) {
            if !line.contains("libc::kill(") {
                continue;
            }
            // The liveness probe, which sends nothing.
            if line.contains(", 0)") {
                continue;
            }
            if path.ends_with("pty_op.rs") {
                continue;
            }
            offenders.push(format!("{path}:{n}: {}", line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "a signal is sent outside the op runner, so a checkpoint at the op queue \
         would not see it:\n  {}",
        offenders.join("\n  ")
    );
}

/// The op runner does send one, or the test above passes by there being no
/// signalling at all.
#[test]
fn the_op_runner_is_where_a_signal_is_actually_sent() {
    let text = std::fs::read_to_string(format!("{PLUGINS}/pty_op.rs"))
        .expect("the op runner is where it was");
    assert!(
        production_lines(&text)
            .iter()
            .any(|(_, l)| l.contains("libc::kill(") && !l.contains(", 0)")),
        "nothing in pty_op.rs sends a signal any more -- either it moved, and the \
         test above now passes for the wrong reason, or the capability is gone"
    );
}

/// Bytes reach a pane's pty through `PtyIo`, which is what the op runner
/// drives. A plugin calling the host's inject proxy directly would be a
/// second route.
#[test]
fn bytes_reach_a_pane_only_through_the_io_the_op_runner_drives() {
    let mut offenders = Vec::new();
    for (path, text) in plugin_sources() {
        let lines = production_lines(&text);
        for (i, (n, line)) in lines.iter().enumerate() {
            if !line.contains(".inject_input(") {
                continue;
            }
            // The one forwarder: `ShelldClient::send_input_to`, which
            // `PtyIo::send` calls and nothing else does. Recognise it by
            // the function it sits in rather than by line number.
            let in_forwarder = lines[..i]
                .iter()
                .rev()
                .take(12)
                .any(|(_, l)| l.contains("fn send_input_to"));
            if in_forwarder {
                continue;
            }
            offenders.push(format!("{path}:{n}: {}", line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "bytes are written to a pane outside the one forwarder the op runner drives:\n  {}",
        offenders.join("\n  ")
    );
}

/// And that forwarder is reached from `PtyIo::send` -- the op runner's
/// interface -- rather than from a plugin directly.
#[test]
fn the_forwarder_is_called_from_the_op_runners_interface() {
    let text = std::fs::read_to_string(format!("{PLUGINS}/claudecode/mod.rs"))
        .expect("the client is where it was");
    let lines = production_lines(&text);
    let callers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (_, l))| l.contains("self.send_input_to("))
        .map(|(i, _)| i)
        .collect();
    assert!(
        !callers.is_empty(),
        "nothing calls send_input_to -- the forwarder is now unreachable, or renamed"
    );
    for i in callers {
        let within_pty_io_send = lines[..i]
            .iter()
            .rev()
            .take(6)
            .any(|(_, l)| l.contains("fn send(&self"));
        assert!(
            within_pty_io_send,
            "{}: send_input_to is called from somewhere other than PtyIo::send, which \
             is a second route into a pane",
            lines[i].0
        );
    }
}
