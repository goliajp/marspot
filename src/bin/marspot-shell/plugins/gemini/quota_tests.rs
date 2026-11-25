//! The refusal texts below are gemini's own wording: the `error` record
//! is what its stream hook records for a stopped turn (`[API Error: …]`
//! around the server's message), the dialogs are its quota, overage and
//! empty-wallet dialogs as drawn on a terminal.

use super::*;

/// 2026-11-25T10:00:00Z, a Wednesday in Pacific standard time.
const NOW: i64 = 1_795_600_800;

fn iso(s: &str) -> i64 {
    marspot::cc_usage::parse_iso_utc(s.to_string()).unwrap()
}

#[test]
fn gemini_quota_the_daily_quota_error_is_a_refusal_and_capacity_is_not() {
    let daily = "[API Error: You have exhausted your daily quota on this model.]";
    let r = from_error(daily, NOW, Some("gemini-3-pro-preview")).expect("a refusal");
    assert_eq!(r.scope, Scope::Model("gemini-3-pro-preview".into()), "the session's model");
    assert_eq!((r.how, r.resets_at), (How::Error, iso("2026-11-26T08:00:00Z")), "the next Pacific midnight");

    let reason = r#"[API Error: {"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"}]}}]"#;
    assert_eq!(from_error(reason, NOW, None).unwrap().scope, Scope::Any, "no model known: the whole account");
    let per_day = "[API Error: Quota exceeded for quota metric 'GenerateRequestsPerDayPerProjectPerModel' (Status: RESOURCE_EXHAUSTED)]";
    assert!(from_error(per_day, NOW, None).is_some());

    let after = "[API Error: You have exhausted your daily quota on this model. Your quota will reset after 2h31m5s.]";
    assert_eq!(from_error(after, NOW, None).unwrap().resets_at, NOW + 2 * 3600 + 31 * 60 + 5);

    let full = "[API Error: You have exhausted your capacity on this model. MODEL_CAPACITY_EXHAUSTED]";
    assert_eq!(from_error(full, NOW, None), None, "a full server is not this account's quota");
    assert_eq!(from_error("[API Error: fetch failed]", NOW, None), None);
}

#[test]
fn gemini_quota_a_fallback_switch_refuses_the_model_before_it() {
    let r = from_info("Switched to fallback model gemini-3-flash-preview", NOW, Some("gemini-3-pro-preview")).unwrap();
    assert_eq!(r.scope, Scope::Model("gemini-3-pro-preview".into()));
    assert_eq!((r.how, r.resets_at), (How::Fallback, iso("2026-11-26T08:00:00Z")));
    assert_eq!(from_info("Switched to fallback model x", NOW, None).unwrap().scope, Scope::Any);
    assert_eq!(from_info("Model set to pro", NOW, None), None);
}

const PLAIN: &str = "\
╭──────────────────────────────────────────────╮
│ Usage limit reached for all Pro models.       │
│ Access resets at 4:30 PM PST.                 │
│ /stats model for usage details                │
│ /model to switch models.                      │
│ ● 1. Switch to gemini-3-flash-preview         │
│   2. Stop                                     │
╰──────────────────────────────────────────────╯";

const OVERAGE: &str = "\
│ Usage limit reached for gemini-3.1-pro-preview.          │
│ Access resets at 9:05 AM GMT+9.                          │
│ /auth to switch to API key.                              │
│ You have 120 AI Credits available.                       │
│ How would you like to proceed?                           │
│ ● 1. Use AI Credits - Continue this request (Overage)    │
│   2. Manage - View balance and purchase more credits     │";

const EMPTY: &str = "\
│ Usage limit reached for gemini-2.5-pro.                  │
│ To continue using this model now, purchase more AI Credits. │
│ ● 1. Get AI Credits - Open browser to purchase credits   │
│   2. Stop - Abort request                                │";

#[test]
fn gemini_quota_the_dialogs_on_screen_are_refusals_and_say_which() {
    let plain = from_screen(PLAIN, NOW).unwrap();
    assert_eq!((plain.scope.clone(), plain.how), (Scope::Pro, How::Dialog));
    assert_eq!(plain.resets_at, iso("2026-11-26T00:30:00Z"), "4:30 PM PST after 2 AM PST");
    assert!(plain.scope.covers(Some("gemini-3-pro-preview")) && !plain.scope.covers(Some("gemini-3-flash-preview")));

    let over = from_screen(OVERAGE, NOW).unwrap();
    assert_eq!((over.scope.clone(), over.how), (Scope::Model("gemini-3.1-pro-preview".into()), How::Overage));
    assert_eq!(over.resets_at, iso("2026-11-26T00:05:00Z"), "9:05 AM in GMT+9, the next one after 7 PM there");

    let empty = from_screen(EMPTY, NOW).unwrap();
    assert_eq!((empty.how, empty.resets_at), (How::EmptyWallet, iso("2026-11-26T08:00:00Z")));
    assert_eq!(from_screen("> write the tests\n  gemini-3-pro | 12% context", NOW), None);
}

#[test]
fn gemini_quota_pacific_midnight_follows_daylight_time() {
    assert_eq!(pacific_midnight_after(iso("2026-07-01T12:00:00Z")), iso("2026-07-02T07:00:00Z"));
    assert_eq!(pacific_midnight_after(iso("2026-11-26T07:59:59Z")), iso("2026-11-26T08:00:00Z"));
    assert_eq!(pacific_midnight_after(iso("2026-11-26T08:00:00Z")), iso("2026-11-27T08:00:00Z"));
    // the night daylight time starts and the night it ends
    assert_eq!(pacific_midnight_after(iso("2026-03-08T09:00:00Z")), iso("2026-03-09T07:00:00Z"));
    assert_eq!(pacific_midnight_after(iso("2026-11-01T09:00:00Z")), iso("2026-11-02T08:00:00Z"));
}

#[test]
fn gemini_quota_a_scope_reads_back_and_covers_what_it_should() {
    for s in [Scope::Any, Scope::Pro, Scope::Model("gemini-2.5-flash".into())] {
        assert_eq!(Scope::from_word(&s.word()), s);
    }
    let flash = Scope::Model("gemini-2.5-flash".into());
    assert!(flash.covers(None) && flash.covers(Some("auto")) && flash.covers(Some("flash")), "flash may run gemini-2.5-flash");
    assert!(!flash.covers(Some("gemini-2.5-pro")));
    assert!(!flash.covers(Some("gemini-2.5-flash-lite")) && !flash.covers(Some("flash-lite")), "flash-lite is another model");
    assert!(!Scope::Model("gemini-2.5-flash-lite".into()).covers(Some("gemini-2.5-flash")));
    for router in ["auto-gemini-3", "auto-gemini-2.5"] {
        assert!(Scope::Model("gemini-2.5-pro".into()).covers(Some(router)), "{router} picks its own model");
    }
    let pro = Scope::Model("gemini-3.1-pro-preview".into());
    assert!(pro.covers(Some("pro")) && pro.covers(Some("gemini-3.1-pro-preview-customtools")));
    assert!(!pro.covers(Some("gemini-3-pro-preview")));
    assert!(Scope::Pro.covers(Some("pro")) && Scope::Pro.covers(Some("gemini-2.5-pro")) && !Scope::Pro.covers(Some("flash")));
    assert!(Scope::Any.covers(Some("gemini-2.5-pro")));
    // a refusal of what a router picked names no model
    assert_eq!(from_info("Switched to fallback model x", NOW, Some("auto-gemini-3")).unwrap().scope, Scope::Any);
}

#[test]
fn gemini_quota_a_boxed_or_wrapped_model_name_is_the_name_alone() {
    let dialog = |rows: &str| format!("{rows}\n│ ● 1. Stop                    │\n╰──────────────────────────────╯");
    let boxed = from_screen(&dialog("│ Usage limit reached for gemini-2.5-pro.   │"), NOW).unwrap();
    assert_eq!(boxed.scope, Scope::Model("gemini-2.5-pro".into()));
    let wrapped = from_screen(&dialog("│ Usage limit reached for gemini-3.1-pro- │\n│ preview. Access resets at 4:30 PM PST.  │"), NOW).unwrap();
    assert_eq!(wrapped.scope, Scope::Model("gemini-3.1-pro-preview".into()));
    assert_eq!(wrapped.scope.word(), "gemini-3.1-pro-preview", "one word, as the rows keep it");
    // a whole name at the row's end, without its full stop: the next row is another sentence
    let whole = from_screen(&dialog("│ Usage limit reached for gemini-3-pro-preview │\n│ Access resets at 4:30 PM PST.            │"), NOW).unwrap();
    assert_eq!(whole.scope, Scope::Model("gemini-3-pro-preview".into()));
    let pro = from_screen(&dialog("│ Usage limit reached for all Pro │\n│ models.                         │"), NOW).unwrap();
    assert_eq!(pro.scope, Scope::Pro);
}
