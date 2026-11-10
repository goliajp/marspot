//! What a clicked link hands to `open(1)`.

/// Does this raw scanned-IP text look like a bare, unbracketed IPv6
/// address that needs `[...]` wrapping before it can appear in an
/// `http://` URL?  Bracketed input (`[::1]:8080`) is already URL-safe
/// and returns false.  Bare IPv4 (`1.2.3.4`, `1.2.3.4:8080`) has at
/// most one `:` and returns false.  Bare IPv6 (`::1`, `2001:db8::1`)
/// has `::` or `≥3` colons and returns true.
fn ip_text_is_bare_ipv6(text: &str) -> bool {
    if text.starts_with('[') || !text.contains(':') {
        return false;
    }
    if text.contains("::") {
        return true;
    }
    text.chars().filter(|&c| c == ':').count() >= 3
}

/// What `open(1)` should be handed for a clicked link, or `None` when
/// the kind has no open action.
///
/// A free function so it can be tested: the caller is buried in the
/// context-menu event path, and the one case that actually broke was
/// invisible from there.
pub fn open_arg_for(kind: crate::grid_links::LinkKind, text: &str) -> Option<String> {
    let link_text = text;
    match kind {
        crate::grid_links::LinkKind::Email => {
            Some(format!("mailto:{}", link_text))
        }
        // Bare IP (`47.96.114.231`, `::1`,
        // `2001:db8::1`, `[fe80::1]:8080/foo`) needs an
        // `http://` scheme so `open(1)` routes it —
        // defaults to port 80.  Bare IPv6 without
        // brackets gets wrapped so URL parsers accept
        // it (colons in a hostname are ambiguous with
        // `host:port` otherwise).
        crate::grid_links::LinkKind::Ip => {
            Some(if link_text.starts_with('[')
                || !ip_text_is_bare_ipv6(link_text)
            {
                format!("http://{}", link_text)
            } else {
                format!("http://[{}]", link_text)
            })
        }
        crate::grid_links::LinkKind::Url => {
            let head: String = link_text
                .chars()
                .take(8)
                .flat_map(char::to_lowercase)
                .collect();
            Some(
                if head.starts_with("http://")
                    || head.starts_with("https://")
                {
                    link_text.to_string()
                } else {
                    format!("http://{}", link_text)
                },
            )
        }
        // UUID has no Open action (menu doesn't offer
        // it); if a stale click reaches here, no-op.
        crate::grid_links::LinkKind::Uuid => None,
        // Expand `~` here, not at the call site: `open(1)` does not do
        // shell expansion, so it read `~` as a directory name under the
        // process cwd and reported the file missing — silently, since
        // `spawn` only fails when the process cannot START.  Every
        // `~/…` file link underlined correctly (linkify expands for its
        // stat) and then did nothing when clicked.
        crate::grid_links::LinkKind::File => {
            // A `file://` link carries the scheme in its text so the
            // selection matches what is drawn; `open(1)` wants the
            // path, and percent escapes have to come back off first
            // (`file://…/a%20b` names `a b`).
            let raw = link_text
                .strip_prefix("file://")
                .map(percent_decode)
                .unwrap_or_else(|| link_text.to_string());
            crate::grid_links::expand_user_path(&raw)
                .map(|p| p.to_string_lossy().into_owned())
        }
    }
}

/// Undo percent-escapes in a `file://` URL's path.
///
/// Only `%XX` is handled, and an invalid escape is left as written —
/// a filename may legitimately contain a bare `%`, and mangling it
/// would turn a working link into a missing file.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod ip_bracket_tests {
    use super::ip_text_is_bare_ipv6;
    #[test]
    fn bracketed_and_ipv4_do_not_need_wrapping() {
        assert!(!ip_text_is_bare_ipv6("[::1]:8080"));
        assert!(!ip_text_is_bare_ipv6("47.96.114.231"));
        assert!(!ip_text_is_bare_ipv6("192.168.1.1:8080"));
        assert!(!ip_text_is_bare_ipv6("10.0.0.5/status"));
    }
    #[test]
    fn bare_ipv6_needs_wrapping() {
        assert!(ip_text_is_bare_ipv6("::1"));
        assert!(ip_text_is_bare_ipv6("2001:db8::1"));
        assert!(ip_text_is_bare_ipv6(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334"
        ));
    }
}
