//! The API errors worth a retry, recognised through colour codes and wrapping.

use super::*;

#[test]
fn strip_ansi_drops_csi_and_osc() {
    // Bold colour around the dot + a plain message.
    let raw = b"\x1b[1;33m\xe2\x8f\xba\x1b[0m API Error";
    let stripped = strip_ansi(raw);
    assert_eq!(&stripped, b"\xe2\x8f\xba API Error");
}

#[test]
fn retryable_kind_matches_rate_limit_marker_wrapped() {
    // Real claude output — error wraps after "Rate" onto the
    // next line at a 2-space indent.  Our scanner has to
    // whitespace-normalise to catch it.
    let buf = b"\xe2\x8f\xba API Error: Server is temporarily limiting requests (not your usage limit) \xc2\xb7 Rate\n  limited\n";
    assert_eq!(retryable_error_kind(buf), Some("rate_limited"));
}

#[test]
fn retryable_kind_matches_rate_limit_marker_inline() {
    let buf = b"API Error: ... Rate limited\n";
    assert_eq!(retryable_error_kind(buf), Some("rate_limited"));
}

#[test]
fn retryable_kind_matches_overloaded() {
    let line = b"API Error: overloaded_error";
    assert_eq!(retryable_error_kind(line), Some("overloaded"));
}

#[test]
fn retryable_kind_matches_network() {
    let line = b"API Error: Network error contacting Anthropic";
    assert_eq!(retryable_error_kind(line), Some("network"));
}

#[test]
fn retryable_kind_ignores_unrelated_lines() {
    let line = b"  ok, processing your request ...";
    assert_eq!(retryable_error_kind(line), None);
}

#[test]
fn retryable_kind_survives_ansi_around_marker() {
    let line = b"\x1b[31mAPI Error: Rate limited\x1b[0m\r";
    assert_eq!(retryable_error_kind(line), Some("rate_limited"));
}
