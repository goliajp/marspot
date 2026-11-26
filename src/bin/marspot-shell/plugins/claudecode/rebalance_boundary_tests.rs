use super::*;

const P7: Target = Target { from: 3, to: 7, word: "waste" };

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Pane 481 on 2026-11-26 from 18:30:47Z: resting 152 s with a move to
/// P7 standing, first inside the minute's settle, then held by work in
/// flight, then working again.
#[test]
fn a_rest_held_throughout_writes_one_line_when_it_ends_naming_each_veto() {
    let mut b = Boundaries::default();
    let t0 = Instant::now();
    let held = |v| Outcome::Held(v);
    assert!(b.look(481, t0, Some(s(0)), Some(P7), held(Veto::Settling)).is_empty());
    assert!(b.look(481, t0 + s(30), Some(s(30)), Some(P7), held(Veto::Settling)).is_empty());
    assert!(b.look(481, t0 + s(61), Some(s(61)), Some(P7), held(Veto::WorkInFlight)).is_empty());
    assert!(b.look(481, t0 + s(152), Some(s(152)), Some(P7), held(Veto::WorkInFlight)).is_empty());
    let lines = b.look(481, t0 + s(153), None, Some(P7), held(Veto::Working));
    assert_eq!(lines, ["sid=481 P3 → P7 (waste) rest_s=152 outcome=held why=work_in_flight seen=settle@0,work_in_flight@61"]);
    assert!(b.look(481, t0 + s(200), None, Some(P7), held(Veto::Working)).is_empty(), "no rest, no line");
}

#[test]
fn a_move_ends_the_rest_with_its_line_at_once() {
    let mut b = Boundaries::default();
    let t0 = Instant::now();
    b.look(441, t0, Some(s(5)), Some(P7), Outcome::Held(Veto::Settling));
    let lines = b.look(441, t0 + s(60), Some(s(65)), Some(P7), Outcome::Moved);
    assert_eq!(lines, ["sid=441 P3 → P7 (waste) rest_s=65 outcome=moved seen=settle@5,moved@65"]);
}

#[test]
fn a_rest_the_sweep_did_not_see_end_is_closed_by_the_next_rest() {
    let mut b = Boundaries::default();
    let t0 = Instant::now();
    b.look(1, t0, Some(s(10)), Some(P7), Outcome::WouldMove);
    // the pane worked and rested again between two looks
    let lines = b.look(1, t0 + s(300), Some(s(20)), Some(P7), Outcome::WouldMove);
    assert_eq!(lines, ["sid=1 P3 → P7 (waste) rest_s=10 outcome=would_move seen=would_move@10"]);
    let lines = b.look(1, t0 + s(400), None, Some(P7), Outcome::Held(Veto::Working));
    assert_eq!(lines, ["sid=1 P3 → P7 (waste) rest_s=20 outcome=would_move seen=would_move@20"]);
}

#[test]
fn a_rest_without_a_standing_target_writes_nothing_and_a_dropped_target_is_named() {
    let mut b = Boundaries::default();
    let t0 = Instant::now();
    assert!(b.look(2, t0, Some(s(90)), None, Outcome::Held(Veto::CoreStays)).is_empty());
    assert!(b.look(2, t0 + s(5), None, None, Outcome::Held(Veto::Working)).is_empty());
    b.look(2, t0 + s(10), Some(s(0)), Some(P7), Outcome::Held(Veto::TranscriptBusy));
    b.look(2, t0 + s(20), Some(s(10)), None, Outcome::Held(Veto::CoreStays));
    let lines = b.look(2, t0 + s(30), None, None, Outcome::Held(Veto::Working));
    assert_eq!(lines, ["sid=2 P3 → P7 (waste) rest_s=10 outcome=held why=core_stays seen=transcript_busy@0,core_stays@10"]);
}

#[test]
fn a_standing_target_lasts_while_the_pane_stays_on_its_account() {
    let mut b = Boundaries::default();
    b.stand(3, P7);
    assert_eq!(b.standing(3, 3), Some(P7));
    assert_eq!(b.standing(3, 7), None, "it moved");
    assert_eq!(b.standing(3, 3), None, "and the target went with it");
    b.stand(3, P7);
    b.drop_target(3);
    assert_eq!(b.standing(3, 3), None);
}

#[test]
fn a_pane_gone_mid_rest_ends_it_and_the_record_of_changes_is_capped() {
    let mut b = Boundaries::default();
    let t0 = Instant::now();
    let flip = [Veto::Settling, Veto::WorkInFlight];
    for i in 0..20u64 {
        b.look(4, t0 + s(i), Some(s(i)), Some(P7), Outcome::Held(flip[(i % 2) as usize]));
    }
    b.stand(4, P7);
    let lines = b.retain(|sid| sid != 4);
    assert_eq!(lines.len(), 1);
    let seen = lines[0].split("seen=").nth(1).unwrap().trim_end_matches(" ended=gone");
    assert_eq!(seen.split(',').count(), SEEN_CAP, "{}", lines[0]);
    assert!(seen.ends_with("work_in_flight@19") && lines[0].ends_with(" ended=gone"), "{}", lines[0]);
    assert_eq!(b.standing(4, 3), None);
    assert!(b.retain(|_| false).is_empty());
}
