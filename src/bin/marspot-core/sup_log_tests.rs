use super::parse_log_event;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The shape `install-local.sh` actually sends.
#[test]
fn log_event_is_recognised_and_keeps_the_whole_detail() {
    let args = argv(&[
        "--log-event",
        "UPDATE_SWAP",
        "single-core",
        "swap",
        "complete",
    ]);
    assert_eq!(
        parse_log_event(&args),
        Some(("UPDATE_SWAP", "single-core swap complete".to_string())),
        "detail is several argv words and must be rejoined"
    );
}

/// Degenerate calls must still be handled here rather than falling
/// through to the boot path, which panics on the missing surface
/// env — that fall-through is the bug this replaced.
#[test]
fn malformed_log_event_calls_never_fall_through() {
    let bare = argv(&["--log-event"]);
    assert_eq!(parse_log_event(&bare), Some(("untagged", String::new())));
    let tag_only = argv(&["--log-event", "TAG"]);
    assert_eq!(parse_log_event(&tag_only), Some(("TAG", String::new())));
}

/// A normal boot must NOT be mistaken for a log-event call.
#[test]
fn boot_argv_is_not_a_log_event() {
    let empty = argv(&[]);
    assert_eq!(parse_log_event(&empty), None);
    let status = argv(&["--status"]);
    assert_eq!(parse_log_event(&status), None);
    let wrong_order = argv(&["TAG", "--log-event"]);
    assert_eq!(parse_log_event(&wrong_order), None);
}
