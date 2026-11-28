use std::fs;
use std::path::Path;

use marspot_agent::sched::switch_cost;

use super::super::super::allocation_prices::{Tokens, weeks};

use super::super::super::allocation_usage::Readers;
use super::*;

const SESSION: &str = include_str!("../../../../../crates/marspot-adapters/src/claude/fixtures/switch_session.jsonl");

fn append(p: &Path, lines: &[&str]) {
    let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
    for l in lines {
        std::io::Write::write_all(&mut f, format!("{l}\n").as_bytes()).unwrap();
    }
}

#[test]
fn allocation_switch_cost_measures_a_switch_from_the_transcript() {
    let dir = std::env::temp_dir().join(format!("marspot-switch-cost-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.jsonl");
    fs::write(&path, "").unwrap();
    let lines: Vec<&str> = SESSION.lines().collect();
    let mut r = Readers::default();
    let mut sc = SwitchCosts::default();
    let now = 1_800_000_000;
    let feed = |r: &mut Readers, sc: &mut SwitchCosts| -> Vec<Sample> { r.read(&path, now).iter().filter_map(|s| sc.spent(7, s)).collect() };
    assert!(feed(&mut r, &mut sc).is_empty());
    // the replies before the switch, the side chain among them
    append(&path, &lines[..8]);
    assert!(feed(&mut r, &mut sc).is_empty());
    let switched = marspot::cc_usage::parse_iso_utc("2026-11-27T10:10:00Z".into()).unwrap();
    sc.bound(7, switched);
    append(&path, &lines[8..11]);
    assert!(feed(&mut r, &mut sc).is_empty(), "two replies after are not enough");
    append(&path, &lines[11..12]);
    let got = feed(&mut r, &mut sc);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].ctx, got[0].rewrite, got[0].regen), (212_005, 210_002, 1_800));
    assert_eq!(sc.fit.n, 1);
    // nothing more until the next switch
    append(&path, &lines[12..]);
    assert!(feed(&mut r, &mut sc).is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn allocation_switch_cost_prices_the_context() {
    let near = |x: f64, y: f64| assert!((x - y).abs() < 1e-12, "{x} vs {y}");
    // unmeasured: the charge of a switch before anything was measured, by context
    let mut fit = Fit::default();
    near(switch_weeks(&fit, Class::Restricted, None), crate::plugins::allocation::waste::SWITCH_RESTRICTED);
    near(switch_weeks(&fit, Class::Plain, Some(920_000)), 2.0 * crate::plugins::allocation::waste::SWITCH_PLAIN);
    // measured: the agent's switch prices are the control plane's table
    let s = Sample { at: 0, ctx: 1_000_000, rewrite: 1_000_000, rewrite_1h: 1_000_000, regen: 10_000, hit_after: 0, hit_base: 990 };
    for _ in 0..switch_cost::FIT_MIN {
        fit.add(&s);
    }
    let after = Tokens { cache_1h: 1_000_000, output: 10_000, ..Tokens::default() };
    let normal = Tokens { cache_read: 1_000_000, ..Tokens::default() };
    for (class, model) in [(Class::Plain, "claude-opus-5-5"), (Class::Restricted, "claude-fable-5-2")] {
        near(switch_weeks(&fit, class, Some(1_000_000)), weeks(model, &after) - weeks(model, &normal));
    }
    assert!(switch_weeks(&fit, Class::Plain, Some(800_000)) > switch_weeks(&fit, Class::Plain, Some(200_000)));
}
