//! A hook on the lines runs type at a shell's prompt.
//!
//! The queue knows nothing of what a line starts; whoever assembles the
//! shell may want to, and registers a hook that is shown each such line
//! when the run is queued and may hand back a step to run right after
//! the line is typed.

use std::sync::Arc;

use super::{CallFn, PtyOp, Step, StepKind};

/// `(pane, run name, line) -> what to run once the line is typed`.
pub type AfterTyped = Arc<dyn Fn(u64, &'static str, &[u8]) -> Option<CallFn> + Send + Sync>;

/// `op` with the hook's step after the first shell line it claims;
/// `start` moved with the steps it indexes.
pub(super) fn insert(hook: Option<&AfterTyped>, sid: u64, mut op: PtyOp, start: usize) -> (PtyOp, usize) {
    let Some(hook) = hook else { return (op, start) };
    let found = op.steps.iter().enumerate().find_map(|(i, s)| match &s.kind {
        StepKind::Send(b) if s.at_shell.is_some() => hook(sid, op.name, b).map(|f| (i, f)),
        _ => None,
    });
    let Some((i, f)) = found else { return (op, start) };
    op.steps.insert(i + 1, Step::call(f).named("after_typed"));
    (op, if start > i { start + 1 } else { start })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn hook() -> AfterTyped {
        Arc::new(|_, _, line: &[u8]| line.starts_with(b"claude").then(|| Arc::new(|| Ok(())) as CallFn))
    }

    /// The step goes right after the line, and a run re-armed past the
    /// line keeps pointing at the step it named.
    #[test]
    fn the_hook_step_follows_the_line_it_claimed() {
        let op = PtyOp::new("x.resume")
            .step(Step::settle(Duration::from_millis(1)).named("park"))
            .shell_line(42, b"claude --resume u --model m\r".to_vec(), "resume")
            .step(Step::settle(Duration::from_millis(1)).named("after"));
        let resume = op.index_of("resume").unwrap();
        let after = op.index_of("after").unwrap();
        let (tagged, start) = insert(Some(&hook()), 7, op.clone(), after);
        assert_eq!(tagged.steps[resume + 1].label, "after_typed");
        assert_eq!(tagged.steps[start].label, "after");
        assert_eq!(insert(Some(&hook()), 7, op.clone(), 0).1, 0);
        assert_eq!(insert(None, 7, op.clone(), 0).0.steps.len(), op.steps.len(), "no hook, nothing added");
        let plain = PtyOp::new("cli.send").step(Step::send(b"ls\r".to_vec()).at_shell(42));
        assert_eq!(insert(Some(&hook()), 7, plain, 0).0.steps.len(), 1, "a line the hook passes on");
    }
}
