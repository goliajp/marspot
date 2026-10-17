//! Bare address recognisers: IPv4, IPv6 and UUID.

use crate::sink::collect_into;
use crate::url::{count_parens, is_url_char};

/// Try to parse `chars[start..]` as a bare IPv4 address, optionally
/// followed by `:port` and/or `/path`.  Returns the exclusive end
/// index on success.  Conservative — the goal is to catch things a
/// user could Cmd-click to open in a browser (`47.96.114.231`,
/// `192.168.1.1:8080`, `10.0.0.1/status`) without also underlining
/// version strings.
///
/// Reject rules:
///   - preceded by alnum / `.` / `-` / `_` / `@` / `/` / `:` — that
///     shape is a version, hostname component, or already-matched URL
///     tail, not a fresh IP boundary
///   - any octet > 255 or with a leading zero on a 2+ digit run
///     (`010.1.2.3` is a shell escape / rare curiosity, and the
///     leading-zero-reject also kills `1.02.3.4`-style version noise)
///   - after the 4 octets, next char is `.` / alnum / `-` / `_` — a
///     5th component or an identifier-continuation = version string
///     (`1.2.3.4.5`, `1.2.3.4-rc1`, `1.2.3.4beta`)
pub(crate) fn try_scan_ipv4_url(chars: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric()
            || matches!(prev, '.' | '-' | '_' | '@' | '/' | ':')
        {
            return None;
        }
    }
    let mut i = start;
    for octet in 0..4 {
        if octet > 0 {
            if chars.get(i) != Some(&'.') {
                return None;
            }
            i += 1;
        }
        let d0 = i;
        while i < chars.len() && chars[i].is_ascii_digit() && i - d0 < 3 {
            i += 1;
        }
        let digits = &chars[d0..i];
        if digits.is_empty() {
            return None;
        }
        if digits.len() > 1 && digits[0] == '0' {
            return None;
        }
        let val: u32 = digits
            .iter()
            .map(|c| c.to_digit(10).unwrap())
            .fold(0, |a, d| a * 10 + d);
        if val > 255 {
            return None;
        }
    }
    if let Some(&next) = chars.get(i)
        && (next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_')) {
            return None;
        }
    let ip_end = i;
    if chars.get(i) == Some(&':') {
        let ps = i + 1;
        let mut pe = ps;
        while pe < chars.len() && chars[pe].is_ascii_digit() && pe - ps < 5 {
            pe += 1;
        }
        if pe > ps {
            let port: u32 = chars[ps..pe]
                .iter()
                .map(|c| c.to_digit(10).unwrap())
                .fold(0, |a, d| a * 10 + d);
            if port <= 65535 {
                i = pe;
            }
        }
    }
    if chars.get(i) == Some(&'/') {
        while i < chars.len() && is_url_char(chars[i]) {
            i += 1;
        }
    }
    // Trim like the URL scanner: balanced parens + prose punctuation.
    // The floor is `ip_end`, not `start` — the bare IPv4 body itself
    // must not be shortened by trim (a trailing `.` inside it was
    // already rejected by the octet regex).
    loop {
        if i <= ip_end {
            break;
        }
        let last = chars[i - 1];
        if last == ')' {
            let (o, c) = count_parens(&chars[start..i]);
            if c > o {
                i -= 1;
                continue;
            }
            break;
        }
        if last == '(' {
            let (o, c) = count_parens(&chars[start..i]);
            if o > c {
                i -= 1;
                continue;
            }
            break;
        }
        if matches!(last, ',' | '.' | ';' | ':' | ']' | '}' | '!' | '?') {
            i -= 1;
        } else {
            break;
        }
    }
    Some(i)
}

/// Try to parse `chars[start..]` as an IPv6 address.  Two shapes:
///
///   - Bracketed: `[<ipv6>]` with optional `:port` and `/path` —
///     unambiguous, no false-positive risk
///   - Bare: `<ipv6>` — requires either `::` or the full 8-group form
///     so time strings (`12:34:56`) don't trip it
///
/// Validation delegates to `std::net::Ipv6Addr::from_str`; the local
/// gate just carves out the candidate token from the character stream
/// and enforces the `::`-or-8-group rule for the bare form.
pub(crate) fn try_scan_ipv6(chars: &[char], start: usize, body: &mut String) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric()
            || matches!(prev, '.' | ':' | '-' | '_' | '@' | '/')
        {
            return None;
        }
    }
    if chars.get(start) == Some(&'[') {
        // Bracketed form.  Body is hex + `:` + `.` (for the IPv4-
        // mapped `::ffff:1.2.3.4` shape); anything else in the
        // brackets means it's not an IPv6 literal.
        let body_start = start + 1;
        let mut body_end = body_start;
        while body_end < chars.len() && chars[body_end] != ']' {
            let c = chars[body_end];
            if !(c.is_ascii_hexdigit() || c == ':' || c == '.') {
                return None;
            }
            body_end += 1;
        }
        if chars.get(body_end) != Some(&']') {
            return None;
        }
        collect_into(body, &chars[body_start..body_end]);
        body.parse::<std::net::Ipv6Addr>().ok()?;
        let mut i = body_end + 1;
        if chars.get(i) == Some(&':') {
            let ps = i + 1;
            let mut pe = ps;
            while pe < chars.len() && chars[pe].is_ascii_digit() && pe - ps < 5 {
                pe += 1;
            }
            if pe > ps {
                let port: u32 = chars[ps..pe]
                    .iter()
                    .map(|c| c.to_digit(10).unwrap())
                    .fold(0, |a, d| a * 10 + d);
                if port <= 65535 {
                    i = pe;
                }
            }
        }
        if chars.get(i) == Some(&'/') {
            while i < chars.len() && is_url_char(chars[i]) {
                i += 1;
            }
            // Trim prose punctuation from the path tail.
            while i > body_end + 1 {
                let last = chars[i - 1];
                if matches!(last, ',' | '.' | ';' | ':' | ')' | '}' | '!' | '?') {
                    i -= 1;
                } else {
                    break;
                }
            }
        }
        return Some(i);
    }
    // Bare form: sweep hex + `:` + `.`, then validate + apply the
    // discriminator (contains `::` OR exactly 7 colons = 8 groups).
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if !(c.is_ascii_hexdigit() || c == ':' || c == '.') {
            break;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    // Right boundary — the token must not slide into an identifier.
    if let Some(&next) = chars.get(i)
        && (next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_')) {
            return None;
        }
    collect_into(body, &chars[start..i]);
    let colon_count = body.chars().filter(|&c| c == ':').count();
    // `::` compression is the sharpest IPv6 signal; the 8-group form
    // (7 colons) is the other unambiguous shape.  Everything else
    // falls back to being possibly-time / possibly-URL-port fragment
    // and is rejected here (the URL scanner already ran).
    if !body.contains("::") && colon_count != 7 {
        return None;
    }
    body.parse::<std::net::Ipv6Addr>().ok()?;
    Some(i)
}

/// Canonical UUID: 8-4-4-4-12 hex with dashes.  Anchored at word
/// boundaries so partial matches inside longer identifiers don't
/// trip.  Emits raw text; the click dispatcher offers Copy only.
pub(crate) fn try_scan_uuid(chars: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric() || matches!(prev, '-' | '_' | '.' | ':' | '@' | '/') {
            return None;
        }
    }
    let seg_lens = [8usize, 4, 4, 4, 12];
    let mut i = start;
    for (idx, &want) in seg_lens.iter().enumerate() {
        if idx > 0 {
            if chars.get(i) != Some(&'-') {
                return None;
            }
            i += 1;
        }
        let s0 = i;
        while i < chars.len() && chars[i].is_ascii_hexdigit() && i - s0 < want {
            i += 1;
        }
        if i - s0 != want {
            return None;
        }
    }
    if let Some(&next) = chars.get(i) {
        // `/` — a uuid-named PATH COMPONENT (claude scratchpads,
        // session dirs) is not a standalone UUID token; claiming it
        // here splits the surrounding path into garbage links
        // (2026-07-18 field report).  Mirrors the `/` in the
        // left-boundary reject set.
        if next.is_ascii_alphanumeric() || matches!(next, '-' | '_' | '.' | '/') {
            return None;
        }
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use crate::{LinkKind, scan_line};

    /// 2026-07-16 field ask: bare IPv4 must be recognised as its own
    /// `Ip` kind — OpenLink prepends `http://` (defaults to port 80),
    /// Copy keeps the raw displayed text.  Cover the common shapes:
    /// bare, `:port`, `/path`, both.
    #[test]
    fn bare_ipv4_recognised_as_ip() {
        for (line, expected) in [
            ("connect 47.96.114.231 now", "47.96.114.231"),
            ("admin 192.168.1.1:8080/status ok", "192.168.1.1:8080/status"),
            ("dashboard 10.0.0.5:3000", "10.0.0.5:3000"),
            ("see 8.8.8.8/dns page", "8.8.8.8/dns"),
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert_eq!(ips.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(ips[0].text, expected, "line {line:?}");
        }
    }

    /// URL always wins over Ip — if `http://…` is present, the URL
    /// scanner consumes the string first and the IP scanner never
    /// sees the digits (per user: "如果他形成了 url 就以 url 为准").
    #[test]
    fn url_with_scheme_wins_over_ip() {
        let mut v = Vec::new();
        scan_line("visit http://47.96.114.231:8080/foo done", 0, &mut v);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].kind, LinkKind::Url);
        assert_eq!(v[0].text, "http://47.96.114.231:8080/foo");
    }

    /// IPv4 recogniser must NOT swallow version strings, other
    /// identifiers, or IPs inside an `http://…` URL's tail.
    #[test]
    fn ipv4_rejects_version_strings_and_url_tails() {
        for line in [
            "runtime v1.2.3.4 released",       // preceded by 'v'
            "kernel 1.2.3.4-rc1 in test",      // trailing -rc1
            "matrix 1.2.3.4.5 dot",            // 5th component
            "big 999.1.1.1 not valid",         // octet > 255
            "cargo pkg 1.0.23 pinned",         // not 4 octets
            "leading 010.0.0.1 zero rejected", // leading zero
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert!(
                ips.is_empty(),
                "line {line:?} must not produce a bare-IP: {v:?}"
            );
        }
    }

    /// Prose-wrapping parens around the IP get stripped by the same
    /// balanced-paren trim as URLs.
    #[test]
    fn ipv4_in_prose_parens_trimmed() {
        let mut v = Vec::new();
        scan_line("(see 47.96.114.231:8080)", 0, &mut v);
        let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
        assert_eq!(ips.len(), 1, "{v:?}");
        assert_eq!(ips[0].text, "47.96.114.231:8080");
    }

    /// IPv6 — the three shapes we support: bracketed with port/path,
    /// bare with `::` compression, bare full 8-group form.
    #[test]
    fn ipv6_recognised_as_ip() {
        for (line, expected) in [
            ("localhost ::1 test", "::1"),
            ("connect 2001:db8::1 done", "2001:db8::1"),
            ("bracketed [fe80::1]:8080/foo now", "[fe80::1]:8080/foo"),
            ("bracketed [::1]:22 ssh", "[::1]:22"),
            (
                "full 2001:0db8:85a3:0000:0000:8a2e:0370:7334 ipv6",
                "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
            ),
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert_eq!(ips.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(ips[0].text, expected, "line {line:?}");
        }
    }

    /// Time-of-day, hex identifiers, and other colon-bearing tokens
    /// must NOT match as IPv6 — no `::`, not 8 groups → reject.
    #[test]
    fn ipv6_rejects_time_and_hex_ids() {
        for line in [
            "logged 12:34:56 now",             // time
            "hex abc:def:123 label",           // 3 groups, no ::
            "sha 1a2b3c4d:5e6f7a8b thing",     // 2 groups
            "commit deadbeef today",           // no colon
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert!(ips.is_empty(), "line {line:?}: {v:?}");
        }
    }

    /// UUID canonical form — hex + dashes at fixed positions.
    #[test]
    fn uuid_recognised_as_uuid() {
        for line in [
            "session 550e8400-e29b-41d4-a716-446655440000 opened",
            "trace-id: f47ac10b-58cc-4372-a567-0e02b2c3d479 end",
            "start 00000000-0000-0000-0000-000000000000 nil",
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let uuids: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Uuid).collect();
            assert_eq!(uuids.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(uuids[0].text.len(), 36, "line {line:?}");
        }
    }

    /// UUID reject: wrong segment lengths, non-hex chars, or
    /// left-boundary-inside-identifier.
    #[test]
    fn uuid_rejects_wrong_shapes() {
        for line in [
            "abc123-def456-789012-345678-901234567890 wrong-lens", // 6-6-6-6-12
            "session id-550e8400-e29b-41d4-a716-446655440000 inside", // preceded by '-'
            "not 550e8400e29b41d4a716446655440000 dashless",       // no dashes
            "gh 550e8400-e29b-41d4-a716-44665544000g bad-hex",     // 'g' not hex
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let uuids: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Uuid).collect();
            assert!(uuids.is_empty(), "line {line:?}: {v:?}");
        }
    }

    /// A uuid followed by `/` is a path component, never a Uuid link;
    /// a standalone uuid still links.
    #[test]
    fn uuid_component_in_path_rejected_standalone_still_links() {
        let mut v = Vec::new();
        scan_line("id 3dbde79c-ab6e-43bd-8143-c448617e1d69/scratch x", 0, &mut v);
        assert!(
            v.iter().all(|l| l.kind != LinkKind::Uuid),
            "{v:?}"
        );
        let mut v2 = Vec::new();
        scan_line("id 3dbde79c-ab6e-43bd-8143-c448617e1d69 done", 0, &mut v2);
        assert_eq!(
            v2.iter().filter(|l| l.kind == LinkKind::Uuid).count(),
            1,
            "{v2:?}"
        );
    }
}
