//! Which build a binary is.
//!
//! Every marspot binary carries `MARSPOT_FP=<sha>|<ts>|END` in its
//! rodata.  The form compared everywhere is the marker without the
//! `|END`: the running build's own and one read out of a file must be
//! the same string for the same build.

use std::path::Path;

/// The build this process is.
pub fn own() -> &'static str {
    crate::MARSPOT_FP_TERM
        .strip_suffix("|END")
        .expect("the marker ends in |END")
}

/// The build a binary file is: its `MARSPOT_FP=<sha>|<ts>` marker,
/// without the `|END`.
///
/// The first well-formed marker, not the first `MARSPOT_FP=`: the
/// needle below is itself in the binary's rodata, and where the linker
/// puts it decides which comes first.  A release build merges the two;
/// a debug build lays the needle out on its own ahead of the marker,
/// and reading from there found no fingerprint at all, so a debug L3
/// never handed over.
pub fn of_file(path: &Path) -> Option<String> {
    let buf = std::fs::read(path).ok()?;
    let needle = b"MARSPOT_FP=";
    let mut from = 0;
    while let Some(at) = buf[from..].windows(needle.len()).position(|w| w == needle) {
        let start = from + at;
        let tail = &buf[start..buf.len().min(start + 256)];
        let marker = tail
            .windows(4)
            .position(|w| w == b"|END")
            .and_then(|end| std::str::from_utf8(&tail[..end]).ok())
            .filter(|s| s[needle.len()..].bytes().all(|b| b.is_ascii_graphic()));
        if let Some(s) = marker {
            return Some(s.to_string());
        }
        from = start + needle.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::TempDir;

    /// A debug build holds the bare needle ahead of the marker, with
    /// unrelated rodata after it; the fingerprint is the marker behind.
    #[test]
    fn the_marker_is_found_behind_a_stray_needle() {
        let dir = TempDir::new("build-marker");
        let p = dir.join("debug-layout");
        let mut body = b"MARSPOT_FP=marspot-sessionthere is no such thing\x12\xff\xfe".to_vec();
        body.extend(std::iter::repeat_n(0u8, 4096));
        body.extend_from_slice(b"MARSPOT_FP=unknown|2026-10-17T15:27:04|ENDMARSPOT_LAYER_VERS=x");
        std::fs::write(&p, body).unwrap();
        assert_eq!(
            of_file(&p).as_deref(),
            Some("MARSPOT_FP=unknown|2026-10-17T15:27:04")
        );
    }

    #[test]
    fn this_build_reads_back_as_itself() {
        let dir = TempDir::new("build-marker");
        let p = dir.join("copy");
        let mut body = b"\x7fELF\0".to_vec();
        body.extend_from_slice(crate::MARSPOT_FP_TERM.as_bytes());
        body.extend_from_slice(b"\0rodata\0");
        std::fs::write(&p, body).unwrap();
        assert_eq!(of_file(&p).as_deref(), Some(own()));
        assert_eq!(of_file(&dir.join("absent")), None);
    }
}
