use marspot::shell_proto::StripState;

use super::super::table::Event;
use super::super::tests::{shared_at, temp};
use super::helper_strip_tests::{T, at, begin_at, dir_gone, panes, strips_at};
use super::*;

/// The `+N` after `root`'s title: the shadow panes whose chain of
/// parents ends at it, as the core counts them.
fn title_count(obs: &Observation, root: u64) -> usize {
    let shadows: Vec<u64> = (1..=64).filter(|sid| above(obs, *sid).is_some_and(|p| p != 0)).collect();
    let top = |mut at: u64| {
        while let Some(p) = above(obs, at).filter(|p| *p != 0) {
            at = p;
        }
        at
    };
    shadows.into_iter().filter(|sid| top(*sid) == root).count()
}

/// Rows of `root`'s record for helpers whose panes are open.
fn open_rows(strips: &[HelperStrip], root: u64) -> Vec<(u64, StripState)> {
    strips.iter().filter(|x| x.root_sid == root).flat_map(|x| &x.rows).filter(|r| !r.gone).map(|r| (r.sid, r.state)).collect()
}

#[test]
fn an_idle_helper_keeps_its_row_and_the_title_count_until_it_is_stopped() {
    let d = temp("strip-idle");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    // 2 is a helper of helper 1: both under 10
    begin_at(&s, 1, 10, T);
    begin_at(&s, 2, 1, T + 1);
    let both = panes(&[(10, 0), (1, 10), (2, 1)]);
    strips_at(&s, &mut m, &both, T + 10);
    at(&s, 1, T + 5_000, vec![Event::IdleNoticed]);
    at(&s, 2, T + 6_000, vec![Event::IdleNoticed]);
    for now in [T + 6_000, T + 9_001, T + 3_600_000] {
        let got = strips_at(&s, &mut m, &both, now);
        assert_eq!(open_rows(&got, 10), [(1, StripState::Idle), (2, StripState::Idle)], "at {now}");
        assert_eq!(title_count(&both, 10), open_rows(&got, 10).len());
    }
    assert_eq!(s.lock().batches.next_ms(&s.table()), None, "no deadline while they wait");
    // stopped: its pane closes, and both drop it at once
    at(&s, 2, T + 3_600_001, vec![Event::Stopped { exited: false }]);
    let one = panes(&[(10, 0), (1, 10)]);
    let got = strips_at(&s, &mut m, &one, T + 3_600_001);
    assert_eq!(got[0].rows.iter().map(|r| r.sid).collect::<Vec<_>>(), [1], "no row of it is kept");
    assert_eq!(title_count(&one, 10), 1);
    at(&s, 1, T + 3_600_002, vec![Event::Stopped { exited: false }]);
    let none = panes(&[(10, 0)]);
    assert!(strips_at(&s, &mut m, &none, T + 3_600_002).is_empty());
    assert_eq!(title_count(&none, 10), 0);
    dir_gone(&d);
}

#[test]
fn an_idle_helper_whose_program_exits_says_exited_for_a_moment_with_its_figures_frozen() {
    let d = temp("strip-idle-exit");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    begin_at(&s, 1, 10, T);
    s.used(1, "c", used::Used { tokens: Some(100), had_tool: Some(false), model: None });
    let open = panes(&[(10, 0), (1, 10)]);
    strips_at(&s, &mut m, &open, T + 10);
    at(&s, 1, T + 100, vec![Event::IdleNoticed]);
    assert_eq!(strips_at(&s, &mut m, &open, T + 100)[0].rows[0].state, StripState::Idle);
    at(&s, 1, T + 600_000, vec![Event::ExitedNoticed, Event::Stopped { exited: true }]);
    let left = panes(&[(10, 0)]);
    for now in [T + 600_000, T + 602_999] {
        let got = strips_at(&s, &mut m, &left, now);
        let r = &got[0].rows[0];
        assert_eq!((got[0].rows.len(), r.state, r.gone), (1, StripState::Exited, true), "at {now}");
        assert_eq!((r.ended_ms, r.elapsed_ms(now), r.tokens, r.had_tool), (Some(T + 100), 100, Some(100), false));
    }
    assert_eq!(s.lock().batches.next_ms(&s.table()), Some(T + 603_000));
    assert!(strips_at(&s, &mut m, &left, T + 603_000).is_empty());
    dir_gone(&d);
}
