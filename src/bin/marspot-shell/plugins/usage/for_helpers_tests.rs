use super::*;
use crate::plugins::claudecode::quota::roomiest::Placed;

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

fn number(p: &Pick) -> u8 {
    match p {
        Pick::Account(a) => a.number,
        Pick::NoAccount(_) => panic!("no account: {p:?}"),
    }
}

#[test]
fn helpers_get_the_roomiest_profile_and_its_config_dir() {
    let a = accounts(vec![(1, room(0.7, 0.0)), (2, room(0.1, 0.0)), (3, room(0.5, 0.0))], NOW);
    let got = choose(Agent::Claude, &homes(3), &a, None, &mut Placed::default(), NOW);
    let want = HelperAccount {
        number: 2,
        dir: PathBuf::from("/h/.claude-profile-2"),
        env: vec![("CLAUDE_CONFIG_DIR", "/h/.claude-profile-2".to_string())],
    };
    assert_eq!(got, Pick::Account(want));
}

#[test]
fn helpers_a_batch_of_five_is_spread_and_a_new_feed_starts_over() {
    let a = accounts((1..=6).map(|n| (n, room(0.1 + f64::from(n) * 0.05, 0.0))).collect(), NOW);
    let mut placed = Placed::default();
    let batch: Vec<u8> = (0..5).map(|_| number(&choose(Agent::Claude, &homes(6), &a, None, &mut placed, NOW))).collect();
    assert_eq!(batch, [1, 2, 3, 4, 5]);
    let newer = Accounts { feed_at: NOW + 60, ..a };
    assert_eq!(number(&choose(Agent::Claude, &homes(6), &newer, None, &mut placed, NOW)), 1, "the new feed counts them");
}

#[test]
fn helpers_get_no_account_with_each_recovery_when_every_one_refuses() {
    let a = accounts(vec![(1, room(1.0, 0.0)), (2, room(0.4, 0.99)), (3, logged_out())], NOW);
    let got = choose(Agent::Claude, &homes(3), &a, None, &mut Placed::default(), NOW);
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
    assert_eq!(number(&choose(Agent::Codex, &homes, &a, None, &mut placed, NOW)), 2, "the most of the week left");
    // Spark's five hours are spent on profile 1 and it has no Spark on 2
    let spark = choose(Agent::Codex, &homes[..1], &Accounts { rooms: a.rooms[..1].to_vec(), resets: a.resets[..1].to_vec(), feed_at: a.feed_at }, Some("spark"), &mut placed, NOW);
    assert_eq!(spark, Pick::NoAccount(vec![Recovery { number: 1, at: Some(1_795_003_000) }]));
}
