//! OSC 8 hyperlinks: which targets a program may declare, and what a
//! click on one acts on.
//!
//! `ESC ] 8 ; <params> ; <URI> ST` opens a link and `ESC ] 8 ; ; ST`
//! closes it; the cells written in between belong to it.  Unlike the
//! links `grid_links` finds by their shape, the target is not what is
//! on screen: `ls --hyperlink` shows a file name and means a `file://`
//! URL, a build tool shows a line and means its log.
//!
//! The target comes from whatever program can write to the pty, and a
//! click hands it to `open(1)`, which passes a scheme it does not know
//! to whichever application registered for it.  So the schemes are a
//! fixed list and not a setting: `http`, `https`, `file` and `mailto`.
//! A link to anything else is shown as text and is not a link.

use crate::grid::Grid;
use crate::grid_links::{LinkKind, LinkRange};

/// The longest URI a link may have.  A program that can write to the
/// pty must not be able to make the terminal hold arbitrarily much;
/// 2 KiB is the order of what browsers keep in an address bar.  A
/// longer one is dropped whole, and its text is shown unlinked.
pub const URI_MAX: usize = 2048;

/// Whether `uri` is an absolute URI in one of the schemes a link may
/// have, with nothing in it a URI cannot contain.
pub fn uri_allowed(uri: &str) -> bool {
    if uri.is_empty() || uri.len() > URI_MAX || uri.bytes().any(|b| b <= b' ' || b == 0x7F) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => rest.len() > 2 && rest.starts_with("//"),
        "file" => rest.strip_prefix("//").and_then(|r| r.find('/').map(|i| &r[..i])).is_some_and(host_is_local),
        "mailto" => !rest.is_empty(),
        _ => false,
    }
}

/// Whether the host of a `file://` URL names this machine: empty,
/// `localhost`, or this machine's host name, with or without `.local`.
/// A file on another host -- `ls --hyperlink` run over ssh -- would
/// otherwise open whatever has the same path here.
fn host_is_local(host: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its length
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return false;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let Ok(ours) = std::str::from_utf8(&buf[..len]) else { return false };
    same_host(host, ours)
}

/// `a` and `b` are the same host name, ignoring case and a `.local`
/// suffix either may carry.
fn same_host(a: &str, b: &str) -> bool {
    let bare = |h: &str| -> String {
        let h = h.to_ascii_lowercase();
        h.strip_suffix(".local").map(str::to_string).unwrap_or(h)
    };
    !b.is_empty() && bare(a) == bare(b)
}

/// The declared link under (`col`, `viewport_row`) at `view_offset`, in
/// the shape the click path takes for a link found by its shape.
///
/// `text` is the URI, and `target` what Open and Copy act on: for
/// `mailto:` the address (the opener puts the scheme back), for
/// `file://` the URL with its host dropped, since `open(1)` is handed
/// a path on this machine -- which [`uri_allowed`] has checked the host
/// names.
pub fn link_at(grid: &Grid, view_offset: u16, col: u16, viewport_row: u16) -> Option<LinkRange> {
    let (col_start, col_end, uri) = grid.link_span_at_view(view_offset, col, viewport_row)?;
    if !uri_allowed(&uri) {
        return None;
    }
    let (scheme, rest) = uri.split_once(':')?;
    let (kind, target) = match scheme.to_ascii_lowercase().as_str() {
        "mailto" => (LinkKind::Email, Some(rest.to_string())),
        "file" => {
            let after_host = rest.strip_prefix("//")?;
            let path = &after_host[after_host.find('/')?..];
            (LinkKind::File, Some(format!("file://{path}")))
        }
        _ => (LinkKind::Url, None),
    };
    Some(LinkRange { row: viewport_row, col_start, col_end, kind, text: uri, target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_only_the_four_schemes_are_allowed() {
        for ok in ["https://example.com", "HTTP://x.y/z?q=1", "file:///tmp/a", "file://localhost/tmp/a", "mailto:a@b.c"] {
            assert!(uri_allowed(ok), "{ok}");
        }
        for bad in [
            "",
            "javascript:alert(1)",
            "ssh://host",
            "x-apple.systempreferences:com.apple",
            "vscode://file/x",
            "https:example.com",
            "/tmp/a",
            "example.com",
            "https://a b",
            "https://a\u{7}b",
            "mailto:",
            "file://",
        ] {
            assert!(!uri_allowed(bad), "{bad:?}");
        }
        assert!(!uri_allowed(&format!("https://{}", "a".repeat(URI_MAX))));
    }

    #[test]
    fn osc8_a_file_url_on_another_host_is_not_a_link() {
        for remote in ["file://build-server/tmp/a", "file://10.0.0.2/etc/passwd", "file://localhost.evil/tmp/a"] {
            assert!(!uri_allowed(remote), "{remote}");
        }
        let mut buf = [0u8; 256];
        assert_eq!(unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) }, 0);
        let len = buf.iter().position(|&b| b == 0).unwrap();
        let ours = std::str::from_utf8(&buf[..len]).unwrap();
        assert!(uri_allowed(&format!("file://{ours}/tmp/a")), "{ours}");
        assert!(uri_allowed(&format!("file://{}/tmp/a", ours.to_ascii_uppercase())), "{ours}");
        assert!(uri_allowed("file://LOCALHOST/tmp/a"));
    }

    #[test]
    fn osc8_host_names_match_with_or_without_local() {
        assert!(same_host("studio", "studio.local"));
        assert!(same_host("Studio.local", "studio"));
        assert!(!same_host("studio2", "studio.local"));
        assert!(!same_host("", "studio"));
        assert!(!same_host("studio", ""));
    }
}
