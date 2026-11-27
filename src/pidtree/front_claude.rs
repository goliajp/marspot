//! Whether Claude Code is the program in front of a pane's terminal.

use super::{list_all_procs, proc_argv, proc_row};

/// Whether a process of the foreground group of `shell_pid`'s terminal
/// is Claude Code.  What its screen shows says nothing once it has
/// exited: the last box it drew stays above the shell's next prompt.
///
/// Claude Code is told by its argv[0], whose basename is `claude`
/// however the process renamed itself.  A wrapper that starts it as a
/// child of its own keeps it in the same group, so the whole group is
/// looked at when the leader is not it.
pub fn claude_in_front(shell_pid: i32) -> bool {
    let Some(shell) = proc_row(shell_pid) else { return false };
    let fg = shell.tty_fg_pgid;
    if fg <= 0 {
        return false;
    }
    if fg == shell.pgid {
        return is_claude(shell_pid);
    }
    is_claude(fg) || list_all_procs().iter().any(|p| p.pgid == fg && p.pid != fg && is_claude(p.pid))
}

fn is_claude(pid: i32) -> bool {
    proc_argv(pid)
        .and_then(|argv| argv.into_iter().next())
        .is_some_and(|argv0| argv0.rsplit('/').next() == Some("claude"))
}
