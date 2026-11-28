use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};

fn told(app: &mut CoreApp) -> Vec<u64> {
    std::mem::take(&mut app.pending_to_shell)
        .into_iter()
        .filter(|(t, _)| *t == MsgType::PaneUserInput)
        .map(|(_, p)| marspot::shell_proto::decode_pane_user_input(&p).unwrap())
        .collect()
}

/// A file dropped on a pane is input the shell does not see; it hears
/// of it once a second per pane at most.
#[test]
fn a_dropped_file_is_reported_and_reports_are_spaced_per_pane() {
    let sb = Sandbox::new();
    let mut app = app_in(sb.dir(), vec![win(1, vec![Pane::new_vacant(31, 60, 16)])]);
    app.rebuild_layout(0);
    app.file_drop(0, 10.0, 10.0, &["/tmp/a b".into()]);
    assert_eq!(told(&mut app), vec![31]);
    app.file_drop(0, 10.0, 10.0, &["/tmp/c".into()]);
    assert!(told(&mut app).is_empty(), "the same pane within a second");
    app.user_wrote(32);
    assert_eq!(told(&mut app), vec![32], "another pane at once");
    app.user_told = Told(Some((31, Instant::now() - EVERY)));
    app.file_drop(0, 10.0, 10.0, &["/tmp/d".into()]);
    assert_eq!(told(&mut app), vec![31], "a second on");
}
