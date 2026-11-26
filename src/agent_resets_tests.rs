use super::*;

const NOW: i64 = 1_795_000_000;

fn codex_grant(ends_at: i64) -> Grant {
    Grant {
        kind: "codex_full".into(),
        restores: vec!["seven_day".into()],
        moves_weekly_reset: true,
        left: Some(1),
        total: Some(1),
        use_requires_limit: Some(false),
        starts_at: Some(NOW - 86_400),
        ends_at: Some(ends_at),
        ..Grant::default()
    }
}

fn codex(observed_at: i64) -> AccountResets {
    let resets = Resets {
        grants: vec![codex_grant(NOW + 3_600), codex_grant(NOW + 7_200)],
        ..Resets::none(observed_at, "app_server")
    };
    AccountResets { agent: "codex".into(), profile: 1, name: "Codex 1".into(), resets }
}

#[test]
fn a_written_file_reads_back_as_it_was() {
    let mut claude = AccountResets {
        agent: "claude".into(),
        profile: 3,
        name: "Claude 3".into(),
        resets: Resets {
            outcome: Outcome::Ineligible,
            eligible: Some(false),
            ineligible_reason: Some("surface".into()),
            ..Resets::none(NOW - 60, "limit_reset_screen")
        },
    };
    let mut fable = Grant { kind: "fable".into(), restores: vec!["seven_day_overage_included".into()], ..codex_grant(NOW + 600) };
    fable.gives_back_weekly = true;
    fable.is_next = true;
    fable.usable_now = Some(true);
    fable.unavailable_reason = Some("needs_limit".into());
    let mut with_grant = claude.clone();
    with_grant.profile = 4;
    with_grant.resets.grants = vec![codex_grant(NOW + 60), fable];
    claude.resets.grants.clear();
    let kimi = AccountResets { agent: "kimi".into(), profile: 0, name: "Kimi".into(), resets: Resets::none(NOW, "none") };
    let all = vec![codex(NOW), claude, with_grant, kimi];
    assert_eq!(parse(&render(&all, NOW)), all);
}

#[test]
fn the_block_has_the_contract_fields_and_no_real_ids() {
    let body = render(&[codex(NOW)], NOW);
    let v = json::parse(&body).unwrap();
    let block = v.at(&["accounts"]).unwrap().arr()[0].get("resets").unwrap();
    for key in ["observed_at", "source", "eligible", "ineligible_reason", "next_grant_ref", "grants", "five_hour_reset", "cooldown_until", "auto_use_enabled", "last_reset"] {
        assert!(block.get(key).is_some(), "{key} in {body}");
    }
    let g = &block.get("grants").unwrap().arr()[1];
    assert_eq!(g.str_at("ref"), Some("codex:1:g2"));
    for key in ["kind", "restores", "gives_back_weekly", "moves_weekly_reset", "left", "total", "usable_now", "use_requires_limit", "unavailable_reason", "starts_at", "ends_at", "paid", "price_usd"] {
        assert!(g.get(key).is_some(), "{key} in {body}");
    }
    assert_eq!(block.get("next_grant_ref"), Some(&Value::Null));
}

#[test]
fn ended_grants_are_neither_written_nor_read() {
    let a = codex(NOW);
    let body = render(std::slice::from_ref(&a), NOW + 3_600);
    let back = parse(&body);
    assert_eq!(back[0].resets.grants.len(), 1, "the grant ending at the hour is gone: {body}");
    assert_eq!(back[0].resets.left_total(), 1);
    assert_eq!(back[0].resets.next_expiry(), Some(NOW + 7_200));
    assert_eq!(a.resets.clone().live(NOW + 7_200).grants, Vec::new());
}

#[test]
fn counts_and_the_next_expiry_skip_what_is_used() {
    let mut r = codex(NOW).resets;
    assert_eq!((r.left_total(), r.next_expiry()), (2, Some(NOW + 3_600)));
    r.grants[0].left = Some(0);
    assert_eq!((r.left_total(), r.next_expiry()), (1, Some(NOW + 7_200)));
    assert!(r.same_as(&Resets { observed_at: NOW + 300, ..r.clone() }));
    assert!(!r.same_as(&codex(NOW).resets));
}

#[test]
fn a_devops_block_with_iso_times_and_unknown_fields_reads() {
    let block = r#"{"observed_at":"2026-11-26T03:00:54Z","source":"app_server","eligible":true,"ineligible_reason":null,
        "next_grant_ref":"codex:1:g1","grants":[{"ref":"codex:1:g1","kind":"codex_full","restores":["seven_day"],
        "gives_back_weekly":false,"moves_weekly_reset":true,"left":3,"total":3,"usable_now":true,"use_requires_limit":false,
        "unavailable_reason":null,"starts_at":null,"ends_at":1796070363,"paid":false,"price_usd":null,"extra":1}],
        "five_hour_reset":{"available":true},"future":[1,2]}"#;
    let r = Resets::from_value(&json::parse(block).unwrap()).expect("a block");
    assert_eq!(r.observed_at, crate::cc_usage::parse_iso_utc("2026-11-26T03:00:54Z".into()).unwrap());
    assert_eq!((r.outcome, r.eligible, r.left_total(), r.next_expiry()), (Outcome::Read, Some(true), 3, Some(1_796_070_363)));
    assert!(r.grants[0].is_next && r.grants[0].moves_weekly_reset);
    assert_eq!(r.grants[0].kind, "codex_full");
    assert!(Resets::from_value(&json::parse(r#"{"observed_at":1}"#).unwrap()).is_none(), "no grants list, no block");
    assert!(parse("not json").is_empty());
}

#[test]
fn a_card_shows_one_line_per_kind_with_counts_and_window() {
    let when = |t: i64| format!("t{}", t - NOW);
    let mut r = codex(NOW).resets;
    assert_eq!(lines(&r, NOW, when), ["full 2/2 to t3600"], "a start already behind is not shown");
    r.grants[0].left = Some(0);
    r.grants[1].usable_now = Some(false);
    assert_eq!(lines(&r, NOW, when), ["full 1/2 to t7200 not now"], "the used one says nothing of the window");
    r.grants.push(Grant { kind: "fable".into(), left: Some(1), ends_at: Some(NOW + 60), ..Grant::default() });
    r.grants.push(Grant { kind: "full".into(), left: Some(2), starts_at: Some(NOW + 5), ..Grant::default() });
    r.grants.push(Grant { kind: "weekly".into(), left: Some(1), ..Grant::default() });
    assert_eq!(lines(&r, NOW, when), ["full 1/2 to t7200 not now", "fable 1 to t60", "full 2 from t5"], "three kinds at most");
    r.grants[3].ends_at = Some(NOW + 50);
    assert_eq!(lines(&r, NOW, when)[2], "full 2 to t50 from t5", "the end first, so a narrow card cuts the start");
    assert_eq!(line_count(&r), 3);
    assert!(lines(&Resets::none(NOW, "none"), NOW, when).is_empty(), "Kimi and Gemini: nothing");
    let no = Resets { outcome: Outcome::Ineligible, ineligible_reason: Some("surface".into()), ..Resets::none(NOW, "limit_reset_screen") };
    assert_eq!(lines(&no, NOW, when), ["resets: ineligible (surface)"]);
    assert_eq!((line_count(&no), line_count(&Resets::none(NOW, "none"))), (1, 0));
}
