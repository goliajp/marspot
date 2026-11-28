//! The shell is told which pane keys go to whenever that changes, also
//! when only the key window does.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};

fn keyboard_frames(app: &mut CoreApp) -> Vec<Option<u64>> {
    std::mem::take(&mut app.pending_to_shell)
        .into_iter()
        .filter(|(t, _)| *t == MsgType::KeyboardPane)
        .map(|(_, p)| marspot::shell_proto::decode_keyboard_pane(&p).unwrap())
        .collect()
}

#[test]
fn the_keyboard_pane_follows_the_key_window_and_the_app_focus() {
    let sb = Sandbox::new();
    let vacant = |sid| Pane::new_vacant(sid, 60, 16);
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant(11)]), win(2, vec![vacant(22)])]);
    app.renderer.set_window_focused(true);
    app.notify_pane_focus_changes();
    assert_eq!(keyboard_frames(&mut app), vec![Some(11)], "a core that starts says so first");
    app.notify_pane_focus_changes();
    assert!(keyboard_frames(&mut app).is_empty(), "nothing while nothing changed");

    // the other window becomes key: no window's focused pane changed
    app.focus_window(2).unwrap();
    app.notify_pane_focus_changes();
    assert!(app.pending_to_shell.iter().all(|(t, _)| *t != MsgType::PaneFocused), "PaneFocused alone would miss this");
    assert_eq!(keyboard_frames(&mut app), vec![Some(22)]);

    app.renderer.set_window_focused(false);
    app.notify_pane_focus_changes();
    assert_eq!(keyboard_frames(&mut app), vec![None], "keys go to no pane while the app is not active");
}
