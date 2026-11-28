use marspot_agent::sched::switch_cost::{self, Fit, Sample};

use super::turns;

const SESSION: &str = include_str!("fixtures/switch_session.jsonl");

fn at(hms: &str) -> i64 {
    super::super::text::unix_of_iso(&format!("2026-11-27T{hms}Z")).unwrap()
}

#[test]
fn switch_turns_reads_the_main_replies_once_each() {
    let t = turns(SESSION);
    // ten replies of the conversation; the side chain is not one, and the
    // reply written twice counts its larger output
    assert_eq!(t.len(), 10);
    assert_eq!(t[1].output, 350);
    assert!(t.iter().all(|x| x.write_1h != 90_000));
    assert_eq!(t[5].at, at("10:11:00"));
    assert!(t[5].cold() && !t[4].cold());
}

#[test]
fn switch_cost_samples_the_first_reply_after_a_switch() {
    let t = turns(SESSION);
    let s = switch_cost::sample(&t, at("10:10:00"), None).unwrap();
    assert_eq!(s, Sample { at: at("10:11:00"), ctx: 212_005, rewrite: 210_002, rewrite_1h: 210_002, regen: 1_800, hit_after: 0, hit_base: 990 });
    assert_eq!(Sample::parse(s.at, &s.words()), Some(s));
    // the replies after stop at the next switch
    let short = switch_cost::sample(&t, at("10:10:00"), Some(at("10:12:00"))).unwrap();
    assert_eq!(short.regen, 1_400 - 400);
}

#[test]
fn switch_cost_skips_a_switch_after_the_cache_expired() {
    let t = turns(SESSION);
    assert_eq!(switch_cost::sample(&t, at("12:19:00"), None), None);
    // too few replies before the session's start
    assert_eq!(switch_cost::sample(&t, at("10:01:00"), None), None);
}

#[test]
fn switch_cost_fit_is_a_share_of_the_context() {
    let t = turns(SESSION);
    let s = switch_cost::sample(&t, at("10:10:00"), None).unwrap();
    let mut f = Fit::default();
    for _ in 0..switch_cost::FIT_MIN - 1 {
        f.add(&s);
    }
    assert_eq!(f.measured(), None);
    f.add(&s);
    let sh = f.measured().unwrap();
    assert!((sh.rewrite - 210_002.0 / 212_005.0).abs() < 1e-12);
    assert_eq!(sh.one_hour, 1.0);
    assert!((sh.regen - 1_800.0 / 212_005.0).abs() < 1e-12);
    let buckets = switch_cost::by_length(&[s, Sample { ctx: 50_000, ..s }]);
    assert_eq!((buckets[0].n, buckets[1].n, buckets[2].n), (1, 1, 0));
}
