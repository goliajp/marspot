//! The terminal font: which family the grid is drawn in, and at what
//! size.
//!
//! Taste, not correctness: Monaco 12 matches what a user coming from
//! iTerm2 sees, and any other monospaced face is as right for someone
//! else.  The family is stored by name and is not checked against the
//! installed fonts here — this crate draws nothing; a name the machine
//! does not have is drawn in the default instead, and the file keeps
//! the name so installing the font later brings it back.

use super::Settings;
use super::values::{fmt_f32, parse_f32};

pub const DEFAULT_FAMILY: &str = "Monaco";
pub const DEFAULT_SIZE: f32 = 12.0;
/// The sizes a file may name, in points.  Below 8 a cell is too small
/// to read; above 32 a 13-inch screen holds fewer than 50 columns.
pub const SIZE_MIN: f32 = 8.0;
pub const SIZE_MAX: f32 = 32.0;
/// The longest family name a file may give, in bytes: real names are
/// well under 64, and the name is drawn in the panel and handed to
/// CoreText.
pub const FAMILY_MAX: usize = 128;

/// The value of one of the font keys, `None` for any other key.
pub(super) fn value_of(s: &Settings, key: &str) -> Option<String> {
    Some(match key {
        "appearance.font_family" => s.font_family.clone(),
        "appearance.font_size" => fmt_f32(s.font_size),
        _ => return None,
    })
}

/// Apply `key = v` when `key` is one of the font keys; false otherwise.
pub(super) fn parse_key(s: &mut Settings, key: &str, v: &str) -> bool {
    match key {
        "appearance.font_family" => {
            let name = family_name(v);
            if !name.is_empty() {
                s.font_family = name;
            }
        }
        "appearance.font_size" => s.font_size = parse_f32(v, s.font_size, SIZE_MIN, SIZE_MAX),
        _ => return false,
    }
    true
}

/// A family name as the file gives it, without control characters
/// and cut at a character boundary to [`FAMILY_MAX`] bytes.
fn family_name(v: &str) -> String {
    let mut out = String::new();
    for c in v.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > FAMILY_MAX {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::super::*;

    /// Monaco 12 unless the file names another, and what it names comes
    /// back from a rewrite as it was written.
    #[test]
    fn the_font_is_monaco_12_unless_the_file_names_another() {
        let d = Settings::default();
        assert_eq!((d.font_family.as_str(), d.font_size), ("Monaco", 12.0));
        let body = "appearance.font_family = \"JetBrains Mono\"\nappearance.font_size = 13.5\n";
        let s = parse(body);
        assert_eq!(s.font_family, "JetBrains Mono");
        assert!((s.font_size - 13.5).abs() < 1e-6);
        let out = render(&s, body);
        assert!(out.contains("appearance.font_family = JetBrains Mono\n"), "{out}");
        assert!(out.contains("appearance.font_size = 13.5\n"), "{out}");
        assert_eq!(parse(&out), s, "and the round trip is stable");
    }

    /// An empty family or a size outside the range is a slip, and
    /// leaves the default in place rather than drawing nothing.
    #[test]
    fn an_empty_family_or_a_size_out_of_range_keeps_the_default() {
        for body in [
            "appearance.font_family = \n",
            "appearance.font_family = \"\"\n",
            "appearance.font_size = 4\n",
            "appearance.font_size = 64\n",
            "appearance.font_size = big\n",
        ] {
            assert_eq!(parse(body), Settings::default(), "{body:?}");
        }
    }

    /// The family name is the file's input: control characters are
    /// dropped and a runaway name is cut to the cap.
    #[test]
    fn a_family_name_loses_control_characters_and_is_capped() {
        use super::FAMILY_MAX;
        let s = parse("appearance.font_family = Fira\u{7}\u{1b}Code\n");
        assert_eq!(s.font_family, "FiraCode");
        let long = "\u{3042}".repeat(100);
        let s = parse(&format!("appearance.font_family = {long}\n"));
        let n = s.font_family.len();
        assert!(n <= FAMILY_MAX && n > FAMILY_MAX - 3, "{n}");
        assert!(long.starts_with(&s.font_family));
        assert_eq!(parse("appearance.font_family = \u{7}\n"), Settings::default());
    }
}
