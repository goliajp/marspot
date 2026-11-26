//! Stand-in data for `--snapshot --panel cc`.
//!
//! Fixed values, never the live feed: a snapshot whose content moves
//! cannot be compared against the one before it, and comparing is the
//! whole point — the panels are supposed to look like one program.

pub(crate) fn demo_cc_usage(rect: marspot_term::layout::Rect) -> marspot::ui::components::cc_usage_modal::CcUsageRender {
    use marspot::ui::components::cc_usage_modal::{CcUsageAccountRender, CcUsageRender};
    // A fixed instant, so the timeline lands in the same place every
    // run: 2026-01-02 03:00:00 UTC.
    let now = 1_767_322_800i64;
    let win = |label: &str, util: f32, reset: i64, hm: &str, span: i64| {
        marspot::ui::components::cc_usage_modal::CcUsageWindowRender {
            label: label.to_string(),
            util: Some(util),
            reset_unix: Some(reset),
            span_secs: span,
            reset_hm: hm.to_string(),
        }
    };
    let account = |name: &str, email: &str, status: &str, sev: u8, a: f32, b: f32| {
        CcUsageAccountRender {
            name: name.to_string(),
            email: email.to_string(),
            status_label: status.to_string(),
            status_severity: sev,
            resets: Vec::new(),
            reset_label: "reset: 01/02 06:00".to_string(),
            windows: vec![
                win("5h", a, now + 3 * 3600, "06:00", 5 * 3600),
                win("7d", b, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
                win("opus", a, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
                win("sonnet", b * 0.5, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
            ],
        }
    };
    CcUsageRender {
        rect,
        updated_label: "updated 01/02 03:00".to_string(),
        accounts: vec![
            account("primary", "one@example.com", "ok", 0, 0.42, 0.61),
            account("spare", "two@example.com", "near limit", 1, 0.88, 0.35),
        ],
        clock: marspot::ui::components::cc_usage_modal::Clock::fixed(now, 0),
        pointer: None,
        surface: Default::default(),
    }
}
