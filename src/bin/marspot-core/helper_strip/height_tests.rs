use super::*;

const MS: Duration = Duration::from_millis(1);

/// A height and the clock it is stepped by.
struct Rig {
    h: Height,
    t: Instant,
    s: Sight,
}

impl Rig {
    fn new(h: Height) -> Self {
        let now = Instant::now();
        Rig { h, t: now, s: Sight { said: Said::Unknown, inline: false, held: false, typed: None, now } }
    }

    fn at(&mut self, ms: u32) -> Option<u16> {
        self.s.now = self.t + MS * ms;
        self.h.step(&self.s)
    }

    fn rows(&mut self, works: bool, n: usize) {
        self.s.said = Said::Rows { works, need: rows_for(n, false, self.s.inline) };
    }

    fn merged_rows(&mut self, works: bool, n: usize) {
        self.s.said = Said::Rows { works, need: rows_for(n, true, self.s.inline) };
    }

    fn due_ms(&self) -> Option<u128> {
        self.h.due(&self.s).map(|d| (d - self.t).as_millis())
    }
}

#[test]
fn rows_for_helpers() {
    assert_eq!([0, 1, 5, 6, 24].map(|n| rows_for(n, false, false)), [0, 3, 7, 8, 8], "a blank row and main, then them");
    assert_eq!([0, 1, 5, 6, 24].map(|n| rows_for(n, true, false)), [0, 1, 5, 6, 6], "under the agent's own list");
    assert_eq!([0, 1, 4, 5, 6].map(|n| rows_for(n, false, true)), [0, 4, 7, 8, 8], "a blank row under them on the main screen");
    assert_eq!([0, 1, 5].map(|n| rows_for(n, true, true)), [0, 2, 6]);
    assert_eq!([0, 1, 2, 8, 9].map(Height::could_be), [false, true, true, true, false]);
}

#[test]
fn on_the_alternate_screen_it_is_as_high_as_the_rows_drawn() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 2);
    assert_eq!(r.at(0), Some(4));
    r.merged_rows(true, 2);
    assert_eq!(r.at(10), Some(2), "the agent's own list has the head");
    r.merged_rows(true, 3);
    assert_eq!(r.at(20), Some(3));
    r.rows(true, 3);
    assert_eq!(r.at(30), Some(5), "its list went: a head of its own");
}

#[test]
fn on_the_main_screen_it_only_grows() {
    let mut r = Rig::new(Height::default());
    r.s.inline = true;
    r.rows(true, 2);
    assert_eq!(r.at(0), Some(5));
    r.merged_rows(true, 2);
    assert_eq!(r.at(10), None, "each resize leaves rows twice in the history");
    r.merged_rows(true, 5);
    assert_eq!(r.at(20), Some(6));
}

#[test]
fn the_agents_own_list_is_taken_a_second_after_it_came_and_three_after_it_went() {
    let t = Instant::now();
    let mut m = Merge::default();
    m.saw(true, t);
    assert!(m.merged(), "what is there at the first look is taken as it is");
    assert_eq!(m.due(), None);
    m.saw(false, t + MS * 100);
    m.settle(t + MS * 1_600);
    assert!(m.merged(), "a prompt or a menu hides it for a while");
    assert_eq!(m.due(), Some(t + MS * 3_100));
    m.saw(true, t + MS * 2_700);
    m.settle(t + MS * 4_000);
    assert!(m.merged(), "back before three seconds were over");
    assert_eq!(m.due(), None);
    m.saw(false, t + MS * 4_100);
    m.settle(t + MS * 7_000);
    assert!(m.merged());
    m.settle(t + MS * 7_100);
    assert!(!m.merged());
    assert_eq!(m.due(), None, "nothing is owed once it is taken");
    m.saw(true, t + MS * 8_000);
    assert_eq!(m.due(), Some(t + MS * 9_000), "it is taken back a second after it came");
    m.settle(t + MS * 9_000);
    assert!(m.merged());
}

#[test]
fn it_grows_a_step_a_helper_and_goes_once_all_have_ended() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 0);
    assert_eq!(r.at(0), None, "none is ready");
    r.rows(true, 1);
    assert_eq!(r.at(10), Some(3));
    assert_eq!(r.at(11), None);
    r.rows(true, 2);
    assert_eq!(r.at(20), Some(4));
    r.rows(true, 1);
    assert_eq!(r.at(30), Some(3), "one left the list");
    r.rows(false, 1);
    assert_eq!(r.at(1_000), None);
    assert_eq!(r.due_ms(), None, "the shell says when");
    assert_eq!(r.at(3_900), None);
    r.s.said = Said::Nothing;
    assert_eq!(r.at(4_000), Some(0));
    assert_eq!((r.at(9_000), r.due_ms()), (None, None));
}

#[test]
fn one_back_at_work_keeps_it() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 2);
    assert_eq!(r.at(0), Some(4));
    r.rows(false, 2);
    r.at(10);
    r.rows(true, 2);
    r.at(2_000);
    r.s.said = Said::Nothing;
    assert_eq!(r.at(2_100), None, "the shell never said it was over");
    assert_eq!(r.due_ms(), Some(5_100));
    assert_eq!(r.at(5_100), Some(0));
}

#[test]
fn a_batch_first_seen_over_does_not_grow() {
    let mut r = Rig::new(Height::default());
    r.s.said = Said::Rows { works: false, need: 0 };
    assert_eq!(r.at(0), None);
    r.s.inline = true;
    assert_eq!(r.at(5), None, "on the main screen either");
    r.s.said = Said::Nothing;
    assert_eq!(r.at(10), None);
}

#[test]
fn typing_puts_it_off_for_half_a_minute_at_most() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 1);
    r.s.typed = Some(r.t);
    assert_eq!(r.at(500), None);
    assert_eq!(r.due_ms(), Some(1_000));
    assert_eq!(r.at(1_000), Some(3), "a second of quiet");
    r.rows(true, 2);
    for ms in (2_000..32_000).step_by(500) {
        r.s.typed = Some(r.t + MS * ms);
        assert_eq!(r.at(ms), None, "{ms}");
    }
    assert_eq!(r.due_ms(), Some(32_000));
    assert_eq!(r.at(32_000), Some(4));
}

#[test]
fn a_pane_something_else_has_is_left_alone() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 1);
    r.s.held = true;
    assert_eq!((r.at(0), r.due_ms()), (None, None));
    assert_eq!(r.at(60_000), None);
    r.s.held = false;
    assert_eq!(r.at(60_001), Some(3));
    r.s.said = Said::Nothing;
    r.at(60_002);
    r.s.held = true;
    assert_eq!(r.at(70_000), None, "nor given back");
}

#[test]
fn on_the_main_screen_it_is_kept_for_the_next_ones() {
    let mut r = Rig::new(Height::default());
    r.s.inline = true;
    r.rows(true, 1);
    assert_eq!(r.at(0), Some(4));
    r.rows(true, 7);
    assert_eq!(r.at(10), Some(MAX_ROWS));
    r.rows(false, 7);
    r.at(20);
    r.s.said = Said::Nothing;
    assert_eq!((r.at(5_000), r.due_ms()), (None, None));
    r.rows(true, 1);
    assert_eq!(r.at(6_000), None, "there for the next ones");
    r.rows(false, 1);
    r.at(6_100);
    r.s.said = Said::Nothing;
    assert_eq!(r.at(9_000), None);
    r.s.inline = false;
    assert_eq!(r.at(9_100), Some(0), "the alternate screen, or the agent left");
}

#[test]
fn spare_rows_go_with_a_resize() {
    let mut r = Rig::new(Height::default());
    r.s.inline = true;
    r.rows(true, 1);
    r.at(0);
    assert!(!r.h.resized(), "in use");
    r.rows(false, 1);
    r.at(10);
    r.s.said = Said::Nothing;
    r.at(3_100);
    assert!(r.h.resized());
    assert_eq!((r.h.rows(), r.at(4_000)), (0, None));
}

#[test]
fn what_an_earlier_core_left_is_kept_until_the_shell_has_spoken() {
    let mut r = Rig::new(Height::found(5));
    assert_eq!((r.at(0), r.at(60_000), r.due_ms()), (None, None, None));
    r.rows(true, 2);
    assert_eq!(r.at(60_100), Some(4), "the rows drawn, once the shell has spoken");
    let mut r = Rig::new(Height::found(5));
    r.s.said = Said::Nothing;
    assert_eq!(r.at(0), None);
    assert_eq!(r.at(3_000), Some(0));
    let mut r = Rig::new(Height::found(4));
    r.s.said = Said::Rows { works: false, need: 0 };
    assert_eq!(r.at(0), None);
    r.s.said = Said::Nothing;
    assert_eq!(r.at(2_000), Some(0));
    let mut r = Rig::new(Height::found(MAX_ROWS));
    (r.s.inline, r.s.said) = (true, Said::Nothing);
    r.at(0);
    assert_eq!(r.at(4_000), None, "reserved");
}

#[test]
fn rows_the_user_cleared_are_given_back_three_seconds_on() {
    let mut r = Rig::new(Height::default());
    r.rows(true, 1);
    r.at(0);
    r.rows(false, 1);
    r.at(1_000);
    r.s.said = Said::Cleared;
    assert_eq!((r.at(1_500), r.due_ms()), (None, Some(4_000)));
    assert_eq!(r.at(4_000), Some(0));
}
