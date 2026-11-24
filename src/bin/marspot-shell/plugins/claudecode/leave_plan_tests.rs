use super::*;
use marspot::cc_usage::CcModelLimit;

const NOW: i64 = 1_795_530_644;

/// Fable at 90%, the week at a fifth, no speed known yet.
fn p1() -> CcAccount {
    let reset = NOW + 3 * 86_400;
    CcAccount {
        name: "Claude 1".into(),
        email: "p1".into(),
        status: "allowed".into(),
        util_5h: None,
        util_7d: Some(0.2),
        reset_5h: None,
        reset_7d: Some(reset),
        model_limits: vec![CcModelLimit { label: "Fable".into(), util: Some(0.9), reset: Some(reset) }],
        collected_at: NOW,
        limits_as_of: None,
        tier: "default_claude_max_20x".into(),
    }
}

/// Where no speed is measured, only the Fable panes on an account are
/// taken to fill its Fable cap: ten Opus panes on it leave a Fable pane
/// its ten points.
#[test]
fn quota_fable_speed_where_none_is_measured_counts_only_the_fable_panes() {
    let a = p1();
    let rooms = vec![(1u8, super::super::rooms::room_of(&a, NOW))];
    let speeds = Speeds::default();
    let profiles = vec![(1u8, Some("p1".to_string()))];
    let opus: Vec<Taking> = (0..10).map(|sid| (sid, 1u8, Some("opus-5-5"))).collect();
    let plan_opus = plan(&speeds, &rooms, std::slice::from_ref(&a), &profiles, &opus, &HashMap::new(), NOW);
    let h = plan_opus.lasts(1, Some("fable-5"), RHO0);
    // the week: 0.8 / (10 + 1) panes' 0.006 is twelve hours; Fable on its
    // own would be 0.1 * 0.528 / 0.006, near nine
    assert!(h > TARGET_H, "{h}");

    let fable: Vec<Taking> = (0..10).map(|sid| (sid, 1u8, Some("fable-5"))).collect();
    let plan_fable = plan(&speeds, &rooms, std::slice::from_ref(&a), &profiles, &fable, &HashMap::new(), NOW);
    let h = plan_fable.lasts(1, Some("fable-5"), RHO0);
    assert!(h < 1.0, "ten Fable panes fill ten points of Fable within the hour: {h}");
}

/// What the panes sent this pass are counted against where they went.
#[test]
fn quota_placed_panes_count_against_their_target() {
    let a = p1();
    let rooms = vec![(1u8, super::super::rooms::room_of(&a, NOW))];
    let speeds = Speeds::default();
    let profiles = vec![(1u8, Some("p1".to_string()))];
    let mut p = plan(&speeds, &rooms, std::slice::from_ref(&a), &profiles, &[], &HashMap::new(), NOW);
    let before = p.lasts(1, Some("opus-5-5"), 0.1);
    assert!((before - 8.0).abs() < 1e-9, "{before}");
    p.place(1, Some("opus-5-5"), 0.1);
    let after = p.lasts(1, Some("opus-5-5"), 0.1);
    assert!((after - 4.0).abs() < 1e-9, "{after}");
}
