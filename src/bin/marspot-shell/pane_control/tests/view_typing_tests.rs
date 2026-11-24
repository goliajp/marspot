//! What is shown is not changed under the user's typing.

use std::sync::atomic::Ordering;

use marspot::shell_proto::{CAP_PEEK, DoneCode, USER_TYPING_TEXT};
use marspot_peer::control::{ErrorCode, Where};

use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::{Conn, done_reject};
use super::super::ops_pane::place_sid;
use super::{ctx, entry, hub_at, temp};

#[test]
fn pane_control_view_is_not_changed_while_the_user_types_there() {
    let (hub, rx) = hub_at(&temp("view"));
    // with no core to take it, what is not refused is never answered
    drop(rx);
    hub.caps.store(CAP_PEEK, Ordering::Release);
    let shadow = |sid, of| {
        let mut e = entry(sid, "/w");
        e.shadow_of = of;
        e
    };
    let c = ctx(
        CallerKey::Pane(10),
        vec![entry(10, "/w"), shadow(11, 10), shadow(12, 11), entry(20, "/w"), shadow(21, 20)],
        Config::default(),
    );
    let refused = |sid, w, force| {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        place_sid(&mut conn, &c, sid, w, force).unwrap_err().code == ErrorCode::UserTyping
    };
    let unpeek = Where::Unpeek;
    hub.key_pressed();
    // into the host, or into what is shown in its place
    for at in [10, 11, 12] {
        hub.focused(at);
        assert!(refused(11, Where::Peek, false) && refused(12, Where::Peek, false), "{at}");
        assert!(!refused(21, Where::Peek, false), "{at}: another window pane's");
        assert!(!refused(11, Where::Peek, true), "{at}: forced");
        assert!(refused(10, unpeek, false), "{at}: whatever is shown there");
        assert!(!refused(20, unpeek, false));
        assert_eq!(refused(11, unpeek, false), at == 11, "{at}");
        assert!(!refused(11, unpeek, true));
    }
    hub.last_key_ms.store(0, Ordering::Relaxed);
    assert!(!refused(11, Where::Peek, false), "nothing typed");
    assert_eq!(done_reject(DoneCode::Other, USER_TYPING_TEXT).code, ErrorCode::UserTyping, "nor while it waits in the core");
}
