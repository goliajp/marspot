use super::room::*;

/// The feed's numbers for P1 on 2026-10-03, when an Opus pane put
/// there by hand was moved off it six seconds later: both account
/// windows with room, Fable spent, and the collector's word
/// `rejected` -- which it writes when any window is full, the
/// per-model ones included.
fn fable_spent_but_open(reset_7d: i64) -> Room {
    Room::Known {
        status: "rejected".into(),
        util_5h: Some(0.0),
        util_7d: Some(0.77),
        reset_7d,
        models: vec![("fable".to_string(), Some(1.0))],
        models_measured_at: i64::MAX,
        measured_at: i64::MAX,
    }
}


fn room(util_5h: f64, util_7d: f64, reset_7d: i64, models: &[(&str, f64)]) -> Room {
    Room::Known {
        status: "allowed".into(),
        util_5h: Some(util_5h),
        util_7d: Some(util_7d),
        reset_7d,
        models: models.iter().map(|(l, u)| (l.to_string(), Some(*u))).collect(),
        // Measured now, in the tests that are not about staleness.
        models_measured_at: i64::MAX, measured_at: i64::MAX,
    }
}

fn fable_refusal(at: i64) -> Refusal {
    Refusal { at, model: "fable".into(), logged_out: false, window: Window::Week }
}

/// A room whose numbers were measured at `measured`, with room to
/// spare by every figure the feed reports.
fn roomy_measured_at(measured: i64) -> Room {
    Room::Known {
        status: "allowed_warning".into(),
        util_5h: Some(0.85),
        util_7d: Some(0.88),
        reset_7d: 500,
        models: vec![("fable".to_string(), Some(0.62))],
        models_measured_at: measured,
        measured_at: measured,
    }
}

/// The pane had just been switched to Opus when the Fable refusal
/// it had received earlier moved it (2026-10-03, pane 442). A
/// refusal is about the model it names.
#[test]
fn a_refusal_for_one_model_says_nothing_about_another() {
    let r = roomy_measured_at(1_000);
    let fable = fable_refusal(2_000);
    assert!(r.refused_for_now(Some("fable-5-1"), Some(&fable)));
    assert!(!r.refused_for_now(Some("opus-5-5"), Some(&fable)), "an Opus pane was not refused");
    assert!(!r.refused_for_now(None, Some(&fable)), "no model given: Fable is not assumed");
    let credits = Refusal { at: 2_000, model: "opus-5-5".into(), logged_out: false, window: Window::Week };
    assert!(r.refused_for_now(Some("opus-5-5"), Some(&credits)));
    assert!(!r.refused_for_now(Some("fable-5-1"), Some(&credits)));
    let unnamed = Refusal { at: 2_000, model: String::new(), logged_out: false, window: Window::Week };
    assert!(r.refused_for_now(Some("opus-5-5"), Some(&unnamed)), "unread is taken as covering it");
}

/// The case this exists for, with the real numbers: the feed says
/// 0.62 of Fable is gone and the API says the account is out.
///
/// The feed is not wrong, it is old -- it carried that row's model
/// caps forward from a sample taken 94 minutes earlier, while
/// fourteen panes it had just sent there spent what was left.
#[test]
fn a_refusal_newer_than_the_numbers_is_believed_over_them() {
    let feed_at = 1_790_850_134; // 10:22, when the caps were measured
    let refused_at = 1_790_855_512; // 11:51, when the API said no
    let r = roomy_measured_at(feed_at);
    assert!(
        !r.refused_for(Some("fable-5")),
        "by the feed alone there is room, which is why nothing moved"
    );
    assert!(
        r.refused_for_now(Some("fable-5"), Some(&fable_refusal(refused_at))),
        "but the pane was refused after those numbers were taken"
    );
}

/// The other half, and the reason this is not a second trigger
/// that can disagree with the first: a refusal older than the
/// numbers is already accounted for in them.
///
/// Without the comparison -- taking any refusal as proof -- a pane
/// would be moved off an account that has since been re-measured
/// and has room, once per refusal still sitting in its transcript.
#[test]
fn a_refusal_older_than_the_numbers_is_not() {
    let refused_at = 1_790_850_000;
    let feed_at = 1_790_855_000;
    let r = roomy_measured_at(feed_at);
    assert!(
        !r.refused_for_now(Some("fable-5"), Some(&fable_refusal(refused_at))),
        "the feed has looked since, and it says there is room"
    );
    assert!(
        !r.refused_for_now(None, Some(&fable_refusal(refused_at))),
        "and the same for a pane with no model of its own"
    );
}

/// A profile the feed cannot speak for has nothing to be newer
/// than. Being refused there is all the evidence there is.
#[test]
fn a_refusal_where_the_feed_is_silent_counts() {
    assert!(
        Room::Unknown.refused_for_now(Some("fable-5"), Some(&fable_refusal(1))),
        "unknown plus a closed door is a closed door"
    );
    assert!(
        !Room::Unknown.refused_for_now(Some("fable-5"), None),
        "unknown on its own is still not full"
    );
}

/// The model's cap is a way to be out, never a way to be in.
///
/// Fable is bounded by the account's windows too, so a spent week
/// is spent whatever the model row says.
#[test]
fn a_free_model_does_not_rescue_a_spent_account() {
    let spent_week = room(0.10, 1.00, 1_000, &[("fable", 0.00)]);
    assert!(spent_week.refused_for(Some("fable-5")), "the week is gone");
    assert!(spent_week.refused_for(None));
}

/// The badge writes `fable-5`; the feed writes `Fable`.
#[test]
fn the_badge_token_and_the_feed_label_find_each_other() {
    let r = room(0.1, 0.1, 1_000, &[("fable", 1.00)]);
    assert!(r.refused_for(Some("fable-5")), "badge token, feed label");
    assert!(r.refused_for(Some("fable")), "and the bare name");
    assert!(!r.refused_for(Some("opus-5-5")), "a model it does not meter");
    assert!(!r.refused_for(None));
}

/// The last two percent of a week is quota.  P2, P3, P4 and P6 sat
/// at 98% on 2026-11-05 and the API was still answering them.
#[test]
fn a_week_short_of_full_has_room() {
    for used in [0.98, 0.99] {
        let r = room(0.0, used, 1_000, &[("fable", used)]);
        assert!(!r.refused_for(None), "{used}");
        assert!(!r.refused_for(Some("fable-5-1")), "{used} of Fable too");
    }
    assert!(room(0.0, 1.0, 1_000, &[]).refused_for(None), "full is full");
}

#[test]
fn a_rejection_that_only_fable_explains_shuts_out_only_fable_panes() {
    let r = fable_spent_but_open(500);
    assert!(!r.refused_for(Some("opus-5-5")), "an Opus pane has the whole week left here");
    assert!(r.refused_for(Some("fable-5")), "a Fable pane has nothing");
    assert!(!r.refused_for(None), "no model given: Fable is not assumed");
}

/// The word still counts when no capped model accounts for it.
#[test]
fn a_rejection_no_model_explains_shuts_out_every_pane() {
    let plain = Room::Known {
        status: "rejected".into(),
        util_5h: Some(0.10),
        util_7d: Some(0.50),
        reset_7d: 500,
        models: vec![("fable".to_string(), Some(0.30))],
        models_measured_at: i64::MAX,
        measured_at: i64::MAX,
    };
    assert!(plain.refused_for(Some("opus-5-5")));
    let Room::Known { status, util_5h, util_7d, reset_7d, models_measured_at, measured_at, .. } =
        plain
    else {
        unreachable!()
    };
    let no_rows = Room::Known {
        status, util_5h, util_7d, reset_7d, models: Vec::new(), models_measured_at, measured_at,
    };
    assert!(no_rows.refused_for(Some("opus-5-5")));
}
