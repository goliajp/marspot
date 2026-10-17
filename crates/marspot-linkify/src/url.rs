//! URL recognition: the scheme prefix, the terminator scan and the structural host filter.

/// True when `chars[start..]` begins with `prefix`.  All known
/// callers pass ASCII-only literals (`http://`, `https://`), so we
/// compare per byte without first re-collecting `prefix` into a
/// `Vec<char>` — that re-collect was the next hot spot after the
/// per-line `chars: Vec<char>` fix (samply showed ~6 × 10⁶ small
/// allocs/sec from this one function with `prefix.chars().collect()`,
/// realloc churn dominating render).
pub(crate) fn matches_prefix(chars: &[char], start: usize, prefix: &str) -> bool {
    let pbytes = prefix.as_bytes();
    if start + pbytes.len() > chars.len() {
        return false;
    }
    debug_assert!(
        prefix.is_ascii(),
        "matches_prefix fast path assumes ASCII prefix: {prefix:?}"
    );
    for (i, &pb) in pbytes.iter().enumerate() {
        if chars[start + i] as u32 != pb as u32 {
            return false;
        }
    }
    true
}

/// True iff the char immediately before `pos` is alphanumeric.  Used
/// to keep `/` / `~/` mid-word from being mistaken for a path start.
/// Position 0 has no neighbour → treated as a clean boundary.
pub(crate) fn is_left_boundary_alnum(chars: &[char], pos: usize) -> bool {
    if pos == 0 {
        return false;
    }
    chars[pos - 1].is_alphanumeric()
}

/// URL-flavoured terminator scan.  URLs on the wire are pure ASCII
/// — IDN hostnames arrive as `xn--` punycode, UTF-8 path bytes as
/// `%XX` — so ANY non-ASCII char terminates.  The permissive char
/// set inside the ASCII range follows RFC 3986 (unreserved +
/// reserved), minus a small "prose delimiter" blacklist:
///
///   - `<` `>` `"` `'` `` ` `` `|` — quotes / markup / pipes
///
/// Parens are kept inside the scan so Wikipedia's
/// `Rust_(programming_language)` reaches the end intact; the trim
/// stage below then arbitrates balanced vs prose-wrapping parens by
/// counting `(` vs `)` — unbalanced trailing paren = prose, stripped;
/// balanced = URL content, kept.
///
/// The old "everything non-whitespace goes" behaviour swallowed CJK
/// prose that abutted a URL — the 2026-07-15 field report was
/// `…/calendar(刷新一下)。` becoming the URL text, since neither `(`
/// nor CJK chars broke the scan.  The strict char class fixes it at
/// the source: `刷` is non-ASCII → scan stops at `(`, then the trim
/// strips the dangling `(`.
pub(crate) fn scan_until_link_terminator(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        if !is_url_char(chars[i]) {
            break;
        }
        i += 1;
    }
    // Balanced-paren arbitration + prose punctuation trim.  Repeat
    // until nothing fires — sentences end with `link).`, `link!`,
    // `link.`, etc.
    loop {
        if i <= start {
            break;
        }
        let last = chars[i - 1];
        if last == ')' {
            let (opens, closes) = count_parens(&chars[start..i]);
            if closes > opens {
                i -= 1;
                continue;
            }
            break;
        }
        if last == '(' {
            let (opens, closes) = count_parens(&chars[start..i]);
            if opens > closes {
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
    i
}

pub(crate) fn count_parens(span: &[char]) -> (usize, usize) {
    let mut opens = 0usize;
    let mut closes = 0usize;
    for &c in span {
        if c == '(' {
            opens += 1;
        } else if c == ')' {
            closes += 1;
        }
    }
    (opens, closes)
}

pub(crate) fn is_url_char(c: char) -> bool {
    if !c.is_ascii() {
        return false;
    }
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.' | '_' | '~' | ':' | '/' | '?' | '#' | '[' | ']'
                | '@' | '!' | '$' | '&' | '(' | ')' | '*' | '+'
                | ',' | ';' | '=' | '%'
        )
}

/// Structural URL filter.  The `http://` / `https://` prefix is the
/// caller's gate; this checks the rest.  Rejects:
///
///   - hosts that are empty, `localhost`-shaped (no dot, no digits),
///     or just punctuation (`https://.`, `https://-`)
///   - hosts whose first or last byte is a dot or dash
///   - hosts shorter than 3 chars (`https://a` — too short to point
///     anywhere a user typed on purpose)
pub(crate) fn looks_like_url(span: &[char]) -> bool {
    // Find the scheme separator.
    let after_scheme = if matches_prefix(span, 0, "https://") {
        8
    } else if matches_prefix(span, 0, "http://") {
        7
    } else {
        return false;
    };
    if after_scheme >= span.len() {
        return false;
    }
    // Host runs until the next `/`, `?`, `#`, or end.
    let mut host_end = after_scheme;
    while host_end < span.len() {
        let c = span[host_end];
        if c == '/' || c == '?' || c == '#' {
            break;
        }
        host_end += 1;
    }
    let host = &span[after_scheme..host_end];
    if host.len() < 3 {
        return false;
    }
    let first = host[0];
    let last = host[host.len() - 1];
    if first == '.' || first == '-' || last == '.' || last == '-' {
        return false;
    }
    // Host characters must be a sane subset.
    if !host.iter().all(|c| {
        c.is_ascii_alphanumeric() || matches!(*c, '.' | '-' | ':')
    }) {
        return false;
    }
    // Split off an explicit port so the name and the port can be
    // judged separately — which is the whole of the rule below.
    let (name, port) = match host.iter().position(|c| *c == ':') {
        Some(i) => (&host[..i], Some(&host[i + 1..])),
        None => (host, None),
    };
    if name.is_empty() {
        return false;
    }
    let port_ok = match port {
        // `http://host:port` — the placeholder people actually write —
        // stays rejected, because `port` is not a number.
        Some(p) => !p.is_empty() && p.len() <= 5 && p.iter().all(|c| c.is_ascii_digit()),
        None => false,
    };
    // A dotted name is the ordinary case.  An explicit numeric port is
    // the other one, and it is what a dev terminal is full of:
    // `localhost:6014`, `myserver:8080`.  A port is what separates a
    // real address from the `https://x` placeholders this filter exists
    // to reject — those never carry one, and the `http://host:port`
    // people actually type fails the digits test above.
    //
    // 2026-08-07 report: `http://localhost:6014/tools/documents` went
    // unlinked.  The rule was written as "no dot, no digits" and
    // implemented as "no dot" — the digits half was never there.
    //
    // Bare `https://localhost` stays rejected.  That was settled
    // separately (`url_without_dot_in_host_rejected`: it is the shape
    // chat examples take), and a port is all this report needs.
    name.contains(&'.') || port_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{chars, scan};
    use crate::{LinkKind, LinkRange, scan_line};

    #[test]
    fn url_https_ok() {
        let v = scan("see https://example.com/path for more");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::Url);
        assert_eq!(v[0].text, "https://example.com/path");
        assert_eq!(v[0].col_start, 4);
    }

    #[test]
    fn url_trims_trailing_period() {
        let v = scan("Check out https://example.com/path.");
        assert_eq!(v[0].text, "https://example.com/path");
    }

    #[test]
    fn url_trims_trailing_paren() {
        let v = scan("link (https://example.com/a)");
        assert_eq!(v[0].text, "https://example.com/a");
    }

    #[test]
    fn url_without_dot_in_host_rejected() {
        // `https://localhost` and friends — typical chat example
        // form that shouldn't underline.
        assert!(!looks_like_url(&chars("https://localhost")));
        assert!(!looks_like_url(&chars("https://x")));
    }

    #[test]
    fn url_dot_or_dash_at_host_edge_rejected() {
        assert!(!looks_like_url(&chars("https://.example.com")));
        assert!(!looks_like_url(&chars("https://example.com.")));
        assert!(!looks_like_url(&chars("https://-example.com")));
    }

    #[test]
    fn url_garbage_after_scheme_rejected() {
        assert!(!looks_like_url(&chars("https://")));
        assert!(!looks_like_url(&chars("https:///path")));
    }

    /// 2026-08-07 report: `http://localhost:6014/tools/documents` was
    /// not a link.
    ///
    /// The host filter demanded a dot, so every dev-server URL a
    /// terminal is full of failed it.  The rule was *written* as "no
    /// dot, no digits" and *implemented* as "no dot" — the digits half
    /// had never been there.
    ///
    /// There is no filesystem oracle for URLs, so the structure has to
    /// carry the decision — here, an explicit numeric port.  That is
    /// what separates a real address from the `https://x` placeholders
    /// this filter rejects, and it is all this report needs: bare
    /// `https://localhost` is left rejected, as `url_without_dot_in
    /// _host_rejected` settled.
    #[test]
    fn a_dev_server_url_is_a_link_and_a_placeholder_is_not() {
        for good in [
            "http://localhost:6014/tools/documents",
            "http://localhost:3000",
            "http://myserver:8080/api",
            "http://127.0.0.1:6014/x",
            "https://example.com/a",
        ] {
            let v = scan(&format!("打开 {good} 看看"));
            let urls: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::Url).collect();
            assert_eq!(urls.len(), 1, "no link for {good:?}: {v:?}");
            assert_eq!(urls[0].text, good);
        }
        for bad in [
            "https://x",
            "https://foo",
            // Settled separately, and left alone: without a port this
            // is the shape chat examples take.
            "https://localhost",
            // The placeholder people actually write — `port` is not a
            // number, so it is still not an address.
            "http://host:port",
            "https://.",
            "https://-",
        ] {
            let v = scan(&format!("比如 {bad} 之类"));
            assert!(
                v.iter().all(|r| r.kind != LinkKind::Url),
                "{bad:?} must stay unlinked: {v:?}"
            );
        }
    }

    /// URLs keep the permissive terminator set — parens are legal
    /// inside them (Wikipedia article URLs) and balanced pairs must
    /// survive the trim.
    #[test]
    fn url_keeps_parens_inside() {
        let mut v = Vec::new();
        scan_line(
            "see https://en.wikipedia.org/wiki/Rust_(programming_language) ok",
            0,
            &mut v,
        );
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].text,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    /// 2026-07-15 field report: `https://devops.golia.jp/calendar(刷新一下)。`
    /// was underlined as the URL text — `(` and CJK chars weren't
    /// terminators.  Strict ASCII URL char class + balanced-paren trim
    /// cut it back to the real URL.
    #[test]
    fn url_terminates_at_cjk_and_strips_dangling_paren() {
        let mut v = Vec::new();
        scan_line("日历 - https://devops.golia.jp/calendar(刷新一下)。", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1, "{v:?}");
        assert_eq!(urls[0].text, "https://devops.golia.jp/calendar");
    }

    /// URL directly abutting Japanese prose (no `(`) — same fix, no
    /// dangling ASCII to arbitrate; just non-ASCII terminates.
    #[test]
    fn url_terminates_at_bare_cjk() {
        let mut v = Vec::new();
        scan_line("参考 https://example.com/foo は使えます", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1, "{v:?}");
        assert_eq!(urls[0].text, "https://example.com/foo");
    }

    /// Prose-wrapping parens still get stripped (long-standing case).
    #[test]
    fn url_in_prose_parens_still_trimmed() {
        let mut v = Vec::new();
        scan_line("(see https://example.com/x)", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].text, "https://example.com/x");
    }
}
