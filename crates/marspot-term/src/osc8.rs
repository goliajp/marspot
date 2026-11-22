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
        "file" => rest.starts_with("//"),
        "mailto" => !rest.is_empty(),
        _ => false,
    }
}

/// The declared link under (`col`, `viewport_row`) at `view_offset`, in
/// the shape the click path takes for a link found by its shape.
///
/// `text` is the URI, and `target` what Open and Copy act on: for
/// `mailto:` the address (the opener puts the scheme back), for
/// `file://` the URL with its host dropped, since `open(1)` is handed
/// a path on this machine.
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
        for ok in ["https://example.com", "HTTP://x.y/z?q=1", "file:///tmp/a", "file://host/tmp/a", "mailto:a@b.c"] {
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
        ] {
            assert!(!uri_allowed(bad), "{bad:?}");
        }
        assert!(!uri_allowed(&format!("https://{}", "a".repeat(URI_MAX))));
    }
}
