use super::*;
use crate::plugins::claudecode::quota::helper_pick::{Placed, rooms_scene};

const NOW: i64 = 1_795_000_000;

fn room(week_used: f64, five_used: f64) -> Room {
    Room::Known {
        status: "allowed".into(),
        util_5h: Some(five_used),
        util_7d: Some(week_used),
        reset_7d: NOW + 86_400,
        models: Vec::new(),
        models_measured_at: NOW - 60,
        measured_at: NOW - 60,
    }
}

fn logged_out() -> Room {
    let mut r = room(0.1, 0.0);
    if let Room::Known { status, .. } = &mut r {
        *status = "auth_error".into();
    }
    r
}

fn homes(n: u8) -> Vec<(u8, PathBuf)> {
    (1..=n).map(|i| (i, PathBuf::from(format!("/h/.claude-profile-{i}")))).collect()
}

fn accounts(rooms: Vec<(u8, Room)>, feed_at: i64) -> Accounts {
    let resets = rooms.iter().map(|(n, _)| (*n, Resets { five_hour: NOW + 600, week: NOW + 86_400, models: Vec::new() })).collect();
    Accounts { rooms, resets, feed_at }
}

fn renewing(week_used: f64, hours: f64) -> Room {
    let mut r = room(week_used, 0.0);
    if let Room::Known { reset_7d, .. } = &mut r {
        *reset_7d = NOW + (hours * 3600.0) as i64;
    }
    r
}

/// `choose` on `a` read as the core reads rooms, with `placed`.
fn pick(agent: Agent, homes: &[(u8, PathBuf)], a: &Accounts, model: Option<&str>, placed: &mut Placed, now: i64) -> Pick {
    choose(agent, homes, a, &rooms_scene(&a.rooms, &a.resets, model, now), model, placed, now, None)
}

fn number(p: &Pick) -> u8 {
    match p {
        Pick::Account(a) => a.number,
        Pick::NoAccount(_) => panic!("no account: {p:?}"),
    }
}

#[test]
fn helpers_go_where_the_allocation_has_a_gap_and_get_its_config_dir() {
    // P2 has the most left; P3 renews first and has room for the helper.
    // A Claude helper goes by the one objective: on P2 all it uses would
    // lapse, on P3 only its first thirty hours
    let a = accounts(vec![(1, renewing(0.7, 140.0)), (2, renewing(0.1, 150.0)), (3, renewing(0.5, 30.0))], NOW);
    let got = pick(Agent::Claude, &homes(3), &a, None, &mut Placed::default(), NOW);
    let want = HelperAccount {
        number: 2,
        dir: PathBuf::from("/h/.claude-profile-2"),
        env: vec![("CLAUDE_CONFIG_DIR", "/h/.claude-profile-2".to_string())],
    };
    assert_eq!(got, Pick::Account(want));
}

#[test]
fn helpers_a_new_feed_starts_the_tally_over() {
    let a = accounts(vec![(1, renewing(0.7, 140.0)), (2, renewing(0.1, 150.0)), (3, renewing(0.5, 30.0))], NOW);
    let mut placed = Placed::default();
    let first = number(&pick(Agent::Codex, &homes(3), &a, None, &mut placed, NOW));
    (0..4).for_each(|_| _ = pick(Agent::Codex, &homes(3), &a, None, &mut placed, NOW));
    assert_eq!(placed.counts().iter().map(|(_, _, k)| k).sum::<u32>(), 5);
    let newer = Accounts { feed_at: NOW + 60, ..a.clone() };
    assert_eq!(number(&pick(Agent::Codex, &homes(3), &newer, None, &mut placed, NOW + 60)), first, "the new feed counts them");
    assert_eq!(placed.counts().iter().map(|(_, _, k)| k).sum::<u32>(), 1);

    // claude's tally waits for a view of the panes, which a feed is not
    let mut claude = Placed::default();
    _ = pick(Agent::Claude, &homes(3), &a, None, &mut claude, NOW);
    _ = pick(Agent::Claude, &homes(3), &newer, None, &mut claude, NOW + 60);
    assert_eq!(claude.counts().iter().map(|(_, _, k)| k).sum::<u32>(), 2);
}

#[test]
fn helpers_get_no_account_with_each_recovery_when_every_one_refuses() {
    let a = accounts(vec![(1, room(1.0, 0.0)), (2, room(0.4, 0.99)), (3, logged_out())], NOW);
    let got = pick(Agent::Claude, &homes(3), &a, None, &mut Placed::default(), NOW);
    let want = vec![
        Recovery { number: 1, at: Some(NOW + 86_400) },
        Recovery { number: 2, at: Some(NOW + 600) },
        Recovery { number: 3, at: None },
    ];
    assert_eq!(got, Pick::NoAccount(want));
}

#[test]
fn helpers_on_a_default_home_get_no_variable() {
    assert!(account(Agent::Kimi, 0, PathBuf::from("/h/.kimi-code")).env.is_empty());
    assert!(account(Agent::Gemini, 0, PathBuf::from("/h")).env.is_empty());
    assert_eq!(account(Agent::Kimi, 2, PathBuf::from("/k2")).env, [("KIMI_CODE_HOME", "/k2".to_string())]);
    assert_eq!(account(Agent::Codex, 1, PathBuf::from("/c1")).env, [("CODEX_HOME", "/c1".to_string())]);
}

#[test]
fn helpers_on_gemini_take_turns_over_its_homes() {
    let homes = vec![(1, PathBuf::from("/g1")), (2, PathBuf::from("/g2"))];
    let got: Vec<u8> = (0..4).map(|_| number(&take_turns(Agent::Gemini, &homes))).collect();
    assert_eq!(got, [1, 2, 1, 2]);
    assert_eq!(take_turns(Agent::Gemini, &[]), Pick::NoAccount(Vec::new()));
}

#[test]
fn helpers_name_models_as_the_feeds_do() {
    assert_eq!(feed_model(Agent::Codex, Some("GPT-5.3-Codex-Spark")).as_deref(), Some("spark"));
    assert_eq!(feed_model(Agent::Claude, Some("claude-fable-5")).as_deref(), Some("fable-5"));
    assert_eq!(feed_model(Agent::Kimi, None), None);
}

const CODEX_FEED: &str = r#"{"generated_at":"2026-11-20T00:00:00+00:00","accounts":[
  {"name":"Codex 1","email":"a@example.jp","status":"allowed","utilization_5h":null,"utilization_7d":0.9,"reset_5h":null,"reset_7d":1795100000,
   "model_limits":[{"label":"GPT-5.3-Codex-Spark 5h","utilization":1.0,"reset":1795003000,"window_minutes":300},
                   {"label":"GPT-5.3-Codex-Spark 7d","utilization":0.2,"reset":1795100000,"window_minutes":10080}]},
  {"name":"Codex 2","email":"b@example.jp","status":"allowed","utilization_5h":null,"utilization_7d":0.3,"reset_7d":1795200000,"model_limits":[]}]}"#;

#[test]
fn helpers_on_codex_go_by_its_feed() {
    let usage = marspot::cc_usage::parse_codex(CODEX_FEED);
    let a = codex_accounts(&[1, 2, 3], usage, NOW);
    assert_eq!(a.rooms[2].1, Room::Unknown, "no row for profile 3");
    let homes: Vec<(u8, PathBuf)> = (1..=3).map(|n| (n, PathBuf::from(format!("/c{n}")))).collect();
    let mut placed = Placed::default();
    // Codex 1 renews in under 28 hours with a tenth left, which a helper
    // can use up in time; Codex 2 has the most left
    assert_eq!(number(&pick(Agent::Codex, &homes, &a, None, &mut placed, NOW)), 1);
    // Spark's five hours are spent on profile 1 and it has no Spark on 2
    let one = Accounts { rooms: a.rooms[..1].to_vec(), resets: a.resets[..1].to_vec(), feed_at: a.feed_at };
    let spark = pick(Agent::Codex, &homes[..1], &one, Some("spark"), &mut placed, NOW);
    assert_eq!(spark, Pick::NoAccount(vec![Recovery { number: 1, at: Some(1_795_003_000) }]));
}

/// 2026-11-24 15:00Z: Codex 1 has 0.84 left for 46.6 hours, 1.8 points
/// an hour; Codex 2 1.00 for 144.8 hours, 0.69 an hour.
fn codex_example() -> (Vec<(u8, PathBuf)>, Accounts) {
    const NOW_EX: i64 = 1_795_532_400;
    let feed = format!(
        r#"{{"generated_at":"2026-11-24T15:00:00+00:00","accounts":[
  {{"name":"Codex 1","email":"a@example.jp","status":"allowed","utilization_5h":null,"utilization_7d":0.16,"reset_7d":{},"model_limits":[]}},
  {{"name":"Codex 2","email":"b@example.jp","status":"allowed","utilization_5h":null,"utilization_7d":0.0,"reset_7d":{},"model_limits":[]}}]}}"#,
        NOW_EX + (46.6 * 3600.0) as i64,
        NOW_EX + (144.8 * 3600.0) as i64
    );
    let a = codex_accounts(&[1, 2], marspot::cc_usage::parse_codex(&feed), NOW_EX);
    ((1..=2).map(|n| (n, PathBuf::from(format!("/c{n}")))).collect(), a)
}

#[test]
fn helpers_on_codex_go_to_codex_1_until_its_gap_is_full_then_codex_2() {
    let (homes, a) = codex_example();
    let now = a.feed_at;
    let s = rooms_scene(&a.rooms, &a.resets, None, now);
    assert!((s.accounts[0].rate_cap() * 100.0 - 1.8).abs() < 0.01 && (s.accounts[1].rate_cap() * 100.0 - 0.69).abs() < 0.01, "{s:?}");
    let mut placed = Placed::default();
    let got: Vec<u8> = (0..4).map(|_| number(&pick(Agent::Codex, &homes, &a, None, &mut placed, now))).collect();
    assert_eq!(got, [1, 1, 1, 2], "the most left (Codex 2) was where every one went before");
}

#[test]
fn helpers_a_batch_of_five_is_not_all_on_one_account_while_two_have_a_gap() {
    let (homes, a) = codex_example();
    let now = a.feed_at;
    let s = rooms_scene(&a.rooms, &a.resets, None, now);
    // the precondition: with the batch's demand both accounts have a gap
    let p = crate::plugins::allocation::plan::plan(&s.accounts, 0.0, 5.0 * RHO0, None);
    assert!(p.t_n.iter().filter(|t| **t >= RHO0 / 2.0).count() >= 2, "{p:?}");
    let mut placed = Placed::default();
    let got: Vec<u8> = (0..5).map(|_| number(&pick(Agent::Codex, &homes, &a, None, &mut placed, now))).collect();
    assert!(got.iter().any(|n| *n != got[0]), "{got:?}");
}
