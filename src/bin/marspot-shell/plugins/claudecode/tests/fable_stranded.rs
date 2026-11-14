//! 2026-11-13 22:50 UTC: every Fable pane sat on P4, whose Fable cap
//! was spent, while P5 and P8 had Fable left. Refusals both had given
//! a day and a half earlier still counted, and the panes were held as
//! if every account were out until they were moved by hand.

use super::super::seen::{resets_for, Out, Seen};
use super::*;

/// P8 refused Fable at 2026-11-12 04:33:12 UTC, P5 at 16:02:36.
const P8_REFUSED: i64 = 1_794_457_992;
const P5_REFUSED: i64 = 1_794_499_356;
/// 2026-11-12 16:10 UTC, after both, when the feed read both caps full.
const THEN: &str = "2026-11-12T16:10:00+00:00";
/// 2026-11-13 22:50 UTC.
const NOW_ISO: &str = "2026-11-13T22:50:00+00:00";
const NOW: i64 = 1_794_613_800;

/// The eight rows as the feed gave them on the morning of 2026-11-14,
/// which is what it read through the night: (status, week, week's
/// reset, Fable, Fable's reset). P5's and P8's Fable as `then` says.
fn feed(collected: &str, then: bool) -> Vec<marspot::cc_usage::CcAccount> {
    let p58 = |u: &'static str| if then { "1.0" } else { u };
    let p58_status = |s: &'static str| if then { "rejected" } else { s };
    let rows = [
        ("rejected", "0.62", "1795143599", "1.0", "1795143599"),
        ("rejected", "1.0", "1794776399", "0.81", "1794776400"),
        ("rejected", "1.0", "1794718799", "0.72", "1794718799"),
        ("rejected", "0.57", "1795186799", "1.0", "1795186799"),
        (p58_status("allowed"), "0.38", "1795053599", p58("0.45"), "1795053600"),
        ("rejected", "0.56", "1795150799", "1.0", "1795150799"),
        ("rejected", "1.0", "1794888000", "0.62", "1794888000"),
        (p58_status("allowed"), "0.63", "1794978000", p58("0.42"), "1794978000"),
    ];
    let rows: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (status, week, reset, fable, fable_reset))| {
            let n = i + 1;
            format!(
                r#"{{"name":"Claude {n}","email":"p{n}","status":"{status}","utilization_5h":null,
                    "utilization_7d":{week},"reset_5h":null,"reset_7d":{reset},
                    "model_limits":[{{"label":"Fable","utilization":{fable},"reset":{fable_reset},
                    "window_minutes":10080,"is_active":false}}],
                    "tier":"default_claude_max_20x","collected_at":"{collected}"}}"#
            )
        })
        .collect();
    let text = format!(r#"{{"generated_at":"{collected}","accounts":[{}]}}"#, rows.join(","));
    marspot::cc_usage::parse(&text).expect("the feed parses").accounts
}

const PROFILES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

fn email(p: u8) -> Option<String> {
    Some(format!("p{p}"))
}

/// The rooms at `now` from `accounts`, with what panes have seen marked.
fn rooms(seen: &mut Seen, accounts: &[marspot::cc_usage::CcAccount], now: i64) -> Vec<(u8, Room)> {
    let mut rooms = rooms_for(&PROFILES, accounts, email, now);
    seen.mark(&mut rooms, &resets_for(&PROFILES, accounts, email), now);
    rooms
}

/// A Fable pane on P4 with nothing running under it.
fn fable_on_p4(rooms: &[(u8, Room)]) -> Move {
    next_move(&Situation {
        rooms,
        current: 4,
        needs_model: Some("fable-5-2"),
        tool_executing: false,
        work_running: false,
        moves_this_episode: 0,
        last_target: None,
        stranded_for: None,
        since_last_move: None,
        warned: false,
        background_refused: None,
    })
}

fn refused_on_p5_and_p8() -> Seen {
    let mut seen = Seen::default();
    for (p, at) in [(8, P8_REFUSED), (5, P5_REFUSED)] {
        seen.note(p, &Refusal { at, model: "fable".into(), logged_out: false, window: Window::Week });
    }
    seen
}

#[test]
fn a_fable_cap_read_lower_after_a_refusal_has_renewed_and_takes_panes_again() {
    let mut seen = refused_on_p5_and_p8();
    let then = feed(THEN, true);
    let shut = rooms(&mut seen, &then, NOW - 30 * 3600);
    assert_eq!(fable_on_p4(&shut), Move::Hold("every account is out"), "both caps were full then");

    let now = rooms(&mut seen, &feed(NOW_ISO, false), NOW);
    for p in [5, 8] {
        assert!(!now[p - 1].1.refused_for(Some("fable-5-2")), "P{p}: {:?}", now[p - 1].1);
    }
    assert_eq!(fable_on_p4(&now), Move::To(8), "P8's week renews first");
    let written = seen.changed().expect("what the feed read is kept");
    let back = Seen::decode(&written);
    assert_eq!(back.out, seen.out, "and survives a restart: {written}");
}

/// The feed's lag the earlier rule was made for is not undone: a
/// reading short of full that is never followed by a lower one is a
/// sample running behind, and the refusal stands to the week's end.
#[test]
fn a_cap_read_short_of_full_after_a_refusal_is_not_believed_on_its_own() {
    let mut seen = refused_on_p5_and_p8();
    let behind = rooms(&mut seen, &feed(NOW_ISO, false), NOW);
    assert!(behind[7].1.refused_for(Some("fable-5-2")));
    assert_eq!(fable_on_p4(&behind), Move::Hold("every account is out"));
    // read again, the same: nothing has gone down
    let again = rooms(&mut seen, &feed("2026-11-13T22:55:00+00:00", false), NOW + 300);
    assert!(again[7].1.refused_for(Some("fable-5-2")));
    assert!(matches!(seen.out[&(8, "fable".to_string())], Out { renewed: false, .. }));
}
