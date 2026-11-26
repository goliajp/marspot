use super::*;
use marspot::agent_resets::Outcome;

/// 2026-11-27T00:00:00Z.
const NOW: i64 = 1_795_737_600;

/// FIXTURE SLOT: a stand-in written from the grant fields the CLI keeps,
/// not a capture.  Replace it with the text of the real `/limit-reset`
/// screen of one account (addresses removed) and adjust the words in
/// the parser until this test reads it.
const SCREEN: &str = "\
 Limit resets

 ❯ Full reset        2 of 3 left   available now   until 2026-12-01 14:30
   Fable reset       1/1           at your limit   from 2026-11-28 until 2026-12-05
   Weekly reset      1 left        available after cooldown   expires in 3 days

 Esc to go back
";

#[test]
fn each_listed_grant_reads_with_its_kind_counts_and_window() {
    let r = read(SCREEN, NOW);
    assert_eq!((r.outcome, r.eligible, r.source.as_str()), (Outcome::Read, Some(true), "limit_reset_screen"));
    let kinds: Vec<_> = r.grants.iter().map(|g| (g.kind.as_str(), g.left, g.total)).collect();
    assert_eq!(kinds, [("full", Some(2), Some(3)), ("fable", Some(1), Some(1)), ("weekly", Some(1), None)]);

    let full = &r.grants[0];
    assert_eq!((full.usable_now, full.use_requires_limit, full.starts_at), (Some(true), None, None));
    assert_eq!(full.ends_at, marspot::cc_usage::parse_iso_utc("2026-12-01T14:30:00Z".into()));
    assert_eq!(full.restores, ["five_hour", "seven_day"]);

    let fable = &r.grants[1];
    assert!(fable.gives_back_weekly && fable.use_requires_limit == Some(true));
    assert_eq!(fable.starts_at, marspot::cc_usage::parse_iso_utc("2026-11-28T00:00:00Z".into()));
    assert_eq!(fable.ends_at, marspot::cc_usage::parse_iso_utc("2026-12-05T00:00:00Z".into()));

    let weekly = &r.grants[2];
    assert_eq!((weekly.usable_now, weekly.unavailable_reason.as_deref()), (Some(false), Some("cooldown")));
    assert_eq!(weekly.ends_at, Some(NOW + 3 * 86_400));
    assert_eq!(r.left_total(), 4);
}

#[test]
fn a_missing_command_and_an_ineligible_account_are_readings_too() {
    let missing = read("Unknown slash command: limit-reset", NOW);
    assert_eq!((missing.outcome, missing.eligible, missing.grants.len()), (Outcome::CommandMissing, None, 0));
    let no = read("Resets aren't available: this account is not eligible (surface)", NOW);
    assert_eq!((no.outcome, no.eligible, no.ineligible_reason.as_deref()), (Outcome::Ineligible, Some(false), Some("surface")));
    let empty = read(" Limit resets\n\n You have no resets.\n", NOW);
    assert_eq!((empty.outcome, empty.grants.len()), (Outcome::Read, 0));
}

#[test]
fn times_are_read_in_the_forms_the_screen_may_use() {
    assert_eq!(time_in("in 5h", NOW), Some(NOW + 5 * 3_600));
    assert_eq!(time_in("in 2 weeks", NOW), Some(NOW + 14 * 86_400));
    assert_eq!(time_in("in 3 months", NOW), None, "a month has no fixed length");
    assert_eq!(time_in("2026-12-01t09:05 local", NOW), marspot::cc_usage::parse_iso_utc("2026-12-01T09:05:00Z".into()));
    assert_eq!(time_in("soon", NOW), None);
    assert_eq!(counts("full reset 3 remaining"), (Some(3), None));
    assert_eq!(counts("full reset"), (None, None));
}
