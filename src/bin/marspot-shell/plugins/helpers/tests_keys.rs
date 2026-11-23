use std::cell::RefCell;

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::dialogs::{Ask, Dialog, Look};
use super::first_screens::{First, Pressed};
use super::passing::{Keys, take_enter};
use super::tests_passing::{dialog, helper};

type Sent = Option<Result<u64, Reject>>;

/// One try at `d` with `now` on the screen and the pane answering
/// `sent`: what came of it, and the keys that were tried.
fn press(f: &First, d: &Dialog, keys: &mut Keys, now: &Look, sent: impl Fn() -> Sent) -> (Pressed, Vec<Vec<u8>>) {
    let tried = RefCell::new(Vec::new());
    let out = f.press_through(
        d,
        keys,
        || (String::new(), now.clone()),
        |k| {
            tried.borrow_mut().push(k.to_vec());
            sent()
        },
    );
    (out, tried.into_inner())
}

fn refused(code: ErrorCode) -> Sent {
    Some(Err(Reject::new(code, "x")))
}

#[test]
fn helpers_a_key_that_was_not_sent_may_go_at_the_next_pass() {
    let (dir, s, f) = helper("unsent", AgentKind::Claude, "auto");
    let (arrow, enter) = (dialog(Ask::Bypass, 0, ""), dialog(Ask::Bypass, 2, ""));
    let mut keys = Keys::default();
    for code in [ErrorCode::CoreRestarting, ErrorCode::Unknown] {
        let (out, tried) = press(&f, &arrow, &mut keys, &Look::Ask(arrow.clone()), || refused(code));
        assert_eq!((out, tried), (Pressed::NotSent(code.as_str()), vec![b"\x1b[B".to_vec()]), "an arrow");
        keys.unsent();
    }
    for code in [ErrorCode::CoreRestarting, ErrorCode::PaneBusy, ErrorCode::PtyQueueFull, ErrorCode::NotAttached] {
        let (out, tried) = press(&f, &enter, &mut keys, &Look::Ask(enter.clone()), || refused(code));
        assert_eq!((out, tried), (Pressed::NotSent(code.as_str()), vec![b"\r".to_vec()]));
        keys.unsent();
    }
    assert!(matches!(press(&f, &enter, &mut keys, &Look::Ask(enter.clone()), || None).0, Pressed::NotSent(_)), "no socket yet");
    keys.unsent();
    assert!(!keys.ended());
    // it may have gone in: for good, over a restart too
    assert_eq!(press(&f, &enter, &mut keys, &Look::Ask(enter.clone()), || refused(ErrorCode::Unknown)).0, Pressed::Over);
    assert!(!take_enter(&s.dir_of(1), Ask::Bypass));
    let (out, tried) = press(&f, &enter, &mut Keys::default(), &Look::Ask(enter.clone()), || Some(Ok(1)));
    assert_eq!((out, tried.len()), (Pressed::Over, 0));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_no_key_goes_to_a_screen_that_changed() {
    let (dir, s, f) = helper("changed", AgentKind::Claude, "auto");
    let enter = dialog(Ask::Bypass, 2, "");
    let moved = Look::Ask(dialog(Ask::Bypass, 0, ""));
    for now in [Look::Input, Look::Other, moved, Look::Ask(dialog(Ask::Trust, 2, "/x"))] {
        let mut keys = Keys::default();
        let (out, tried) = press(&f, &enter, &mut keys, &now, || Some(Ok(1)));
        assert_eq!((out, tried.len()), (Pressed::NotSent("the screen changed"), 0), "{now:?}");
    }
    assert!(take_enter(&s.dir_of(1), Ask::Bypass), "its Enter is still there");
    std::fs::remove_dir_all(&dir).unwrap();
}
