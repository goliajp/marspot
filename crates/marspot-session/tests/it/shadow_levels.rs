//! Child panes, the session's half of the two-level rule and of a child
//! the user made: a session refuses a parent that is itself a child and
//! refuses to become a child while it has children; a child with no
//! idle timeout keeps no deadline; both hold across an image swap, and
//! a three-level chain left from before the rule is flattened by it.

use std::time::Duration;

use marspot_term::session_registry::{read_session_entry, rewrite_session_entry};
use marspot_term::shell_proto::{DoneCode, SHADOW_IDLE_NEVER};

use crate::self_execv::{Sandbox, count_in_log};
use crate::shadow_session::{exits_within, kill, pane, parent, set_shadow_idle, shadow_of, sid, swap};

const SLEEP: &str = "stty -echo; printf READY; exec sleep 600";

#[test]
fn shadow_levels_a_third_level_is_refused_either_way() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 11);
    let (mut b, rb) = pane(&mut sandbox, 12, SLEEP, Some((sid(11), 0)));
    let (mut c, rc) = parent(&mut sandbox, 13);
    let under_child = set_shadow_idle(&c, 51, sid(12), SHADOW_IDLE_NEVER);
    assert_eq!(under_child, (DoneCode::Other, "the parent is itself a child pane".into()));
    assert_eq!(shadow_of(13), 0, "a refused parent changes nothing");
    let with_children = set_shadow_idle(&a, 52, sid(13), SHADOW_IDLE_NEVER);
    assert_eq!(with_children, (DoneCode::Other, "a pane with child panes cannot become a child pane".into()));
    assert_eq!(shadow_of(11), 0);
    assert!(count_in_log(sandbox.path(), "l3.shadow.third_level_refused") >= 2);
    // a leaf under a top pane is still fine
    assert_eq!(set_shadow_idle(&c, 53, sid(11), SHADOW_IDLE_NEVER).0, DoneCode::Ok);
    assert_eq!(shadow_of(13), sid(11));
    for s in [&mut a, &mut b, &mut c] {
        s.kill();
    }
    for r in [ra, rb, rc] {
        r.delete();
    }
}

/// A child the user made sits idle past what would be its timeout, and
/// still after an image swap; it ends with its parent all the same.
#[test]
fn shadow_levels_a_child_with_no_timeout_outlives_it_across_a_swap() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let (mut a, ra) = parent(&mut sandbox, 21);
    let (mut b, rb) = pane(&mut sandbox, 22, SLEEP, Some((sid(21), 2)));
    assert_eq!(set_shadow_idle(&b, 61, sid(21), SHADOW_IDLE_NEVER).0, DoneCode::Ok);
    assert!(exits_within(&mut b, 4.0).is_none(), "no idle timeout once the user made it a child");
    let _link = swap(&b, &state, 22);
    assert_eq!(shadow_of(22), sid(21), "still a child after the swap");
    assert!(exits_within(&mut b, 4.0).is_none(), "and still no timeout");
    kill(&a);
    exits_within(&mut b, 2.0).expect("it ends with its parent");
    let _ = a.child.wait();
    ra.delete();
    rb.delete();
}

/// A chain three levels deep from before the rule: the new image of the
/// lowest session puts itself under the top one and says so.
#[test]
fn shadow_levels_an_image_swap_flattens_a_third_level() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let (mut a, ra) = parent(&mut sandbox, 31);
    let (mut b, rb) = parent(&mut sandbox, 32);
    let (mut c, rc) = pane(&mut sandbox, 33, SLEEP, Some((sid(32), 0)));
    // b becomes a child as an image before the rule could have made it
    let mut entry = read_session_entry(sid(32)).unwrap();
    entry.shadow_of = sid(31);
    rewrite_session_entry(&entry).unwrap();
    let before = count_in_log(&state, "L3_SHADOW_FLATTENED");
    let _link = swap(&c, &state, 33);
    assert_eq!(shadow_of(33), sid(31), "under the pane at the top after the swap");
    assert_eq!(count_in_log(&state, "L3_SHADOW_FLATTENED"), before + 1, "said once");
    kill(&a);
    exits_within(&mut c, 2.0).expect("it follows its new parent");
    let _ = a.child.wait();
    b.kill();
    std::thread::sleep(Duration::from_millis(50));
    for r in [ra, rb, rc] {
        r.delete();
    }
}
