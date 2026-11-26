use super::*;
use crate::plugins::allocation::plan::plan;

/// What the logger puts before the text of an `alloc.plan` line, at its
/// longest: time, ms, level, component, pid, thread, tag, tabs, newline.
const PREFIX_MAX: usize = 24 + 13 + 5 + 5 + 5 + 12 + "plugin.claudecode.alloc.plan".len() + 8;
const LOGX_MAX_LINE: usize = 480;

fn q(id: u8, r: f64, f: f64, t: f64, open: bool) -> Quant {
    Quant { id, r, f, t, tf: t, open, stale: false }
}

/// The accounts of 2026-11-26T18:27:31Z, whose plan line the logger
/// cut after P7, so P8 never appeared.
fn accounts_1126() -> Vec<Quant> {
    vec![
        q(1, 0.0, 0.0264, 8.6, true),
        q(2, 0.0, 0.0, 74.6, true),
        q(3, 0.06, 0.45408, 58.6, true),
        q(4, 0.0, 0.20592, 20.6, true),
        q(5, 0.92, 0.46992, 151.6, true),
        q(6, 0.0, 0.0, 10.6, true),
        q(7, 0.20, 0.0, 105.6, true),
        q(8, 0.59, 0.4488, 130.6, true),
    ]
}

fn table_1126() -> Vec<(Option<u8>, Class, f64)> {
    vec![
        (Some(3), Class::Restricted, 0.015),
        (Some(3), Class::Plain, 0.006),
        (Some(3), Class::Plain, 0.003),
        (Some(5), Class::Restricted, 0.012),
        (Some(5), Class::Restricted, 0.003),
        (Some(5), Class::Plain, 0.004),
        (Some(7), Class::Plain, 0.002),
        (Some(8), Class::Plain, 0.011),
    ]
}

#[test]
fn allocation_line_names_every_open_account_and_lists_the_others_out_with_their_renewal() {
    let acc = accounts_1126();
    let p = plan(&acc, 0.0096, 0.0446, Some(KAPPA_FABLE));
    let moves = vec![Some("481→P7(waste)".to_string()), None, Some("441→P5(waste)".to_string())];
    let line = plan_line(&acc, &p, (0.0096, 0.0446), &table_1126(), true, &moves);
    assert!(line.len() <= PLAN_MAX, "{} bytes: {line}", line.len());
    assert!(line.starts_with("shortage A=1.0 B=4.5 week_supply=4.8 short_week=1 "), "{line}");
    for open in ["| P3 R6 Φ6 N0 T59 ", "| P5 R92 Φ47 N45 T152 ", "| P7 R20 Φ0 N20 T106 ", "| P8 R59 Φ45 N14 T131 "] {
        assert!(line.contains(open), "{open} in {line}");
    }
    assert!(line.contains(" | out=P1@9h,P6@11h,P4@21h,P2@75h | would move 481→P7(waste) 441→P5(waste)"), "{line}");
}

#[test]
fn allocation_line_for_16_open_accounts_fits_under_the_loggers_cap() {
    const { assert!(PREFIX_MAX + PLAN_MAX <= LOGX_MAX_LINE) };
    let acc: Vec<Quant> = (1..=16).map(|i| q(i, 0.5 + f64::from(i) / 100.0, 0.4, 100.0 + f64::from(i), true)).collect();
    let table: Vec<(Option<u8>, Class, f64)> = (1..=16).flat_map(|i| [(Some(i), Class::Restricted, 0.01), (Some(i), Class::Plain, 0.02)]).collect();
    let moves: Vec<Option<String>> = (0..12).map(|i| Some(format!("{}→P{}(waste)", 400 + i, i % 16 + 1))).collect();
    let p = plan(&acc, 0.16, 0.32, Some(KAPPA_FABLE));
    let line = plan_line(&acc, &p, (0.16, 0.32), &table, true, &moves);
    assert!(line.len() <= PLAN_MAX, "{} bytes: {line}", line.len());
    for i in 1..=16 {
        assert!(line.contains(&format!("P{i} ")), "P{i} in {line}");
    }
    assert!(!line.contains(" out="), "{line}");
    // moves come last: what did not fit is counted, never cut mid-word,
    // and the count is there even when no move fits
    let shown = line.matches("(waste)").count();
    assert!(line.contains(" | would move "), "{line}");
    assert!(line.ends_with(&format!(" +{}", 12 - shown)), "{line}");
    let few = plan_line(&acc[..4], &plan(&acc[..4], 0.04, 0.08, Some(KAPPA_FABLE)), (0.04, 0.08), &table, true, &moves);
    let shown = few.matches("(waste)").count();
    assert!(few.len() <= PLAN_MAX && shown > 0 && few.ends_with(&format!(" +{}", 12 - shown)), "{few}");
}

#[test]
fn allocation_line_drops_detail_before_accounts_and_counts_any_account_left_out() {
    let acc: Vec<Quant> = (1..=40).map(|i| q(i, 0.9, 0.4, 150.0, true)).collect();
    let p = plan(&acc, 0.1, 0.1, Some(KAPPA_FABLE));
    let line = plan_line(&acc, &p, (0.1, 0.1), &[], false, &[]);
    assert!(line.len() <= PLAN_MAX, "{} bytes: {line}", line.len());
    let named = line.matches("@150").count();
    assert!(named < 40 && line.ends_with(&format!(" +{}", 40 - named)), "{line}");
    assert!(line.contains("| P1 90/40@150,P2 90/40@150,"), "least detail, then without the panes: {line}");
}

#[test]
fn allocation_line_counts_the_moves_when_not_one_fits_and_caps_the_accounts_out() {
    let mut acc: Vec<Quant> = (1..=40).map(|i| q(i, 0.9, 0.4, 150.0, true)).collect();
    acc.extend((41..=70).map(|i| q(i, 0.0, 0.0, f64::from(i), true)));
    let moves: Vec<Option<String>> = (0..12).map(|i| Some(format!("{}→P{}(waste)", 400 + i, i + 1))).collect();
    let line = plan_line(&acc, &plan(&acc, 0.1, 0.1, Some(KAPPA_FABLE)), (0.1, 0.1), &[], true, &moves);
    assert!(line.len() <= PLAN_MAX, "{} bytes: {line}", line.len());
    assert!(line.ends_with(" | would move +12"), "{line}");
    let out = line.split(" | out=").nth(1).unwrap().split(" | ").next().unwrap();
    let named = out.matches('@').count();
    assert!(named < 30 && out.ends_with(&format!("+{}", 30 - named)), "{line}");
}
