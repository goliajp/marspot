#!/usr/bin/env python3
"""Generate emoji_presentation.rs and unicode_data.rs from UCD files.

Invoked by bin/regen-unicode-tables.sh.  Not meant to be run directly
unless you're iterating on the codegen — the shell wrapper handles
fetching the source files.
"""

import argparse
import re
import sys
from pathlib import Path


# ---------- UCD parsing ----------

UCD_LINE_RE = re.compile(
    r"^\s*([0-9A-Fa-f]+)(?:\.\.([0-9A-Fa-f]+))?\s*;\s*([^;#]+?)(?:\s*;\s*([^;#]+?))?\s*(?:#.*)?$"
)


def iter_ucd_ranges(path: Path, prop_filter=None, value_field=2):
    """Yield (start_cp, end_cp, value) for each data line.

    `value_field` selects which semicolon-separated column is the
    "value" — 2 for files where the same column names both the property
    AND the value (GraphemeBreakProperty.txt, emoji-data.txt), 3 for
    files where column 2 names the property and column 3 the value
    (DerivedCoreProperties.txt's InCB section).

    `prop_filter`: optional callable `(prop_name) -> bool` used to gate
    which rows are yielded.  `None` means "yield all data rows".
    """
    with path.open() as f:
        for line in f:
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            m = UCD_LINE_RE.match(line)
            if not m:
                continue
            start_hex, end_hex, p2, p3 = m.groups()
            prop = p2.strip()
            value = p3.strip() if p3 else prop
            if value_field == 2:
                value = prop
            elif value_field == 3:
                pass
            else:
                raise ValueError(f"bad value_field {value_field}")
            if prop_filter is not None and not prop_filter(prop):
                continue
            start = int(start_hex, 16)
            end = int(end_hex, 16) if end_hex else start
            yield (start, end, value)


def coalesce(ranges):
    """Merge adjacent / overlapping ranges of the same value."""
    ranges = sorted(ranges)
    out = []
    for start, end, value in ranges:
        if out and out[-1][2] == value and out[-1][1] + 1 >= start:
            out[-1] = (out[-1][0], max(out[-1][1], end), value)
        else:
            out.append((start, end, value))
    return out


# ---------- file headers (kept in code so the codegen is self-contained) ----------


def file_header_doc_lines(version_line):
    return [
        "Source: " + version_line,
        "",
        "Regeneration: bin/regen-unicode-tables.sh.",
    ]


def emit_emoji_presentation(emd: Path, out: Path):
    raw = list(iter_ucd_ranges(emd, prop_filter=lambda p: p == "Emoji_Presentation"))
    ranges = coalesce(raw)
    ver = ""
    for line in emd.read_text().splitlines():
        if line.startswith("# Version:"):
            ver = line.lstrip("# ").rstrip()
            break

    lines = [
        "//! Generated from the Unicode emoji-data.txt (UTS #51) — every",
        "//! codepoint with `Emoji_Presentation=Yes`.  These are the code",
        "//! points that default to emoji (color, em-box-wide) presentation,",
        "//! the canonical definition of \"is this character an emoji\" for",
        "//! the terminal-width purpose.  Characters with Emoji=Yes but",
        "//! Emoji_Presentation=No (e.g. ⚠ U+26A0, ☝ U+261D) default to TEXT",
        "//! presentation — they become emoji only when followed by VS16",
        "//! (U+FE0F), which the grapheme cluster pass handles separately.",
        "//!",
        "//! Source: https://www.unicode.org/Public/UCD/latest/ucd/emoji/emoji-data.txt",
        f"//! {ver}",
        "//!",
        "//! Regeneration: bin/regen-unicode-tables.sh.",
        "",
        "/// Sorted, non-overlapping inclusive ranges `(start, end)` of",
        "/// codepoints with `Emoji_Presentation=Yes`.",
        "pub const EMOJI_PRESENTATION_RANGES: &[(u32, u32)] = &[",
    ]
    for start, end, _ in ranges:
        lines.append(f"    (0x{start:X}, 0x{end:X}),")
    lines += [
        "];",
        "",
        "/// `true` iff `cp` is an Emoji_Presentation=Yes codepoint per UTS #51.",
        "pub fn has_emoji_presentation(cp: u32) -> bool {",
        "    EMOJI_PRESENTATION_RANGES",
        "        .binary_search_by(|&(start, end)| {",
        "            if cp < start { std::cmp::Ordering::Greater }",
        "            else if cp > end { std::cmp::Ordering::Less }",
        "            else { std::cmp::Ordering::Equal }",
        "        })",
        "        .is_ok()",
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::*;",
        "    #[test]",
        "    fn known_presentation_emoji_match_spec() {",
        "        assert!(has_emoji_presentation(0x2705), \"check mark\");",
        "        assert!(has_emoji_presentation(0x274C), \"cross mark\");",
        "        assert!(has_emoji_presentation(0x2B50), \"star\");",
        "        assert!(has_emoji_presentation(0x1F33F), \"herb\");",
        "        assert!(!has_emoji_presentation(0x26A0), \"warning (text default)\");",
        "        assert!(!has_emoji_presentation(0x261D), \"index up (text default)\");",
        "        assert!(!has_emoji_presentation(b\"A\"[0] as u32));",
        "        assert!(!has_emoji_presentation(0x4E2D), \"CJK middle\");",
        "    }",
        "    #[test]",
        "    fn ranges_sorted_and_disjoint() {",
        "        let mut prev_end = 0u32;",
        "        for (i, &(s, e)) in EMOJI_PRESENTATION_RANGES.iter().enumerate() {",
        "            assert!(s <= e, \"range[{}] start > end\", i);",
        "            if i > 0 { assert!(prev_end < s, \"range[{}] overlaps\", i); }",
        "            prev_end = e;",
        "        }",
        "    }",
        "}",
    ]
    out.write_text("\n".join(lines) + "\n")


GBP_ENUM_MAP = {
    "CR": "GBP::CR",
    "LF": "GBP::LF",
    "Control": "GBP::Control",
    "Extend": "GBP::Extend",
    "ZWJ": "GBP::ZWJ",
    "Regional_Indicator": "GBP::RegionalIndicator",
    "Prepend": "GBP::Prepend",
    "SpacingMark": "GBP::SpacingMark",
    "L": "GBP::L",
    "V": "GBP::V",
    "T": "GBP::T",
    "LV": "GBP::LV",
    "LVT": "GBP::LVT",
}

INCB_ENUM_MAP = {
    "Linker": "InCB::Linker",
    "Consonant": "InCB::Consonant",
    "Extend": "InCB::Extend",
}


def emit_unicode_data(gbp: Path, emd: Path, dcp: Path, out: Path):
    # GraphemeBreakProperty.txt: "RANGE ; VALUE #..."; the value column
    # is also the property name, so accept any row whose value is a
    # known GBP variant.
    gbp_raw = list(
        iter_ucd_ranges(
            gbp,
            prop_filter=lambda p: p in GBP_ENUM_MAP,
            value_field=2,
        )
    )
    gbp_ranges = coalesce(gbp_raw)

    extp_raw = list(
        iter_ucd_ranges(
            emd,
            prop_filter=lambda p: p == "Extended_Pictographic",
            value_field=2,
        )
    )
    extp_raw = [(s, e, "Yes") for s, e, _ in extp_raw]
    extp_ranges = coalesce(extp_raw)

    # DerivedCoreProperties.txt InCB section uses
    # "RANGE ; InCB ; VALUE # ..." — property name "InCB" in column 2,
    # value in column 3.
    incb_raw = list(
        iter_ucd_ranges(
            dcp,
            prop_filter=lambda p: p == "InCB",
            value_field=3,
        )
    )
    incb_raw = [(s, e, v) for s, e, v in incb_raw if v in INCB_ENUM_MAP]
    incb_ranges = coalesce(incb_raw)

    def first_match(path, regex):
        for line in path.read_text().splitlines():
            if re.search(regex, line):
                return line.lstrip("# ").rstrip()
        return ""

    ver_gbp = first_match(gbp, r"GraphemeBreakProperty-\d")
    ver_emd = first_match(emd, r"^# Version:")
    ver_dcp = first_match(dcp, r"DerivedCoreProperties-\d")

    lines = [
        "//! Generated Unicode property tables used by the grapheme cluster",
        "//! segmenter (`crate::grapheme`).  Three independent sources:",
        "//!",
        "//! - **Grapheme_Cluster_Break** — UAX #29 property values per codepoint",
        "//!   ([`GBP`], [`gbp`]).  Drives the GB1..GB13 boundary rules.",
        "//! - **Extended_Pictographic** — UTS #51 / emoji-data.txt boolean",
        "//!   ([`is_extended_pictographic`]).  Needed for GB11 (emoji ZWJ glue).",
        "//! - **InCB** — Indic Conjunct Break value, UAX #44 / DerivedCoreProperties",
        "//!   ([`InCB`], [`incb`]).  Drives GB9c so Indic scripts (Devanagari,",
        "//!   Bengali, Gujarati, Malayalam, Oriya, …) don't break inside",
        "//!   virama-glued consonant clusters.",
        "//!",
        "//! Sources:",
        f"//!   {ver_gbp}",
        f"//!   {ver_emd}",
        f"//!   {ver_dcp}",
        "//!",
        "//! Regeneration: bin/regen-unicode-tables.sh.",
        "",
        "/// Grapheme_Cluster_Break property value (UAX #29).  `Other` is the",
        "/// implicit default for every codepoint not explicitly listed.",
        "#[derive(Copy, Clone, PartialEq, Eq, Debug)]",
        "#[repr(u8)]",
        "pub enum GBP {",
        "    Other = 0,",
        "    CR = 1,",
        "    LF = 2,",
        "    Control = 3,",
        "    Extend = 4,",
        "    ZWJ = 5,",
        "    RegionalIndicator = 6,",
        "    Prepend = 7,",
        "    SpacingMark = 8,",
        "    L = 9,",
        "    V = 10,",
        "    T = 11,",
        "    LV = 12,",
        "    LVT = 13,",
        "}",
        "",
        "/// Sorted, non-overlapping `(start, end, GBP)` ranges from",
        "/// GraphemeBreakProperty.txt.  Codepoints outside any range default",
        "/// to [`GBP::Other`].",
        "pub const GRAPHEME_BREAK_PROPERTY: &[(u32, u32, GBP)] = &[",
    ]
    for start, end, value in gbp_ranges:
        lines.append(f"    (0x{start:X}, 0x{end:X}, {GBP_ENUM_MAP[value]}),")
    lines += [
        "];",
        "",
        "/// Look up the Grapheme_Cluster_Break property for a codepoint.",
        "/// Returns `GBP::Other` for any codepoint not explicitly listed.",
        "pub fn gbp(cp: u32) -> GBP {",
        "    match GRAPHEME_BREAK_PROPERTY.binary_search_by(|&(start, end, _)| {",
        "        if cp < start { std::cmp::Ordering::Greater }",
        "        else if cp > end { std::cmp::Ordering::Less }",
        "        else { std::cmp::Ordering::Equal }",
        "    }) {",
        "        Ok(i) => GRAPHEME_BREAK_PROPERTY[i].2,",
        "        Err(_) => GBP::Other,",
        "    }",
        "}",
        "",
        "/// Sorted, non-overlapping inclusive ranges `(start, end)` of codepoints",
        "/// with `Extended_Pictographic=Yes`.  Drives the GB11 rule",
        "/// \"Emoji  Extend* ZWJ × Emoji\" (compound emoji like 👨‍👩‍👧‍👦).",
        "pub const EXTENDED_PICTOGRAPHIC_RANGES: &[(u32, u32)] = &[",
    ]
    for start, end, _ in extp_ranges:
        lines.append(f"    (0x{start:X}, 0x{end:X}),")
    lines += [
        "];",
        "",
        "/// `true` iff `cp` has `Extended_Pictographic=Yes` per UTS #51.",
        "pub fn is_extended_pictographic(cp: u32) -> bool {",
        "    EXTENDED_PICTOGRAPHIC_RANGES",
        "        .binary_search_by(|&(start, end)| {",
        "            if cp < start { std::cmp::Ordering::Greater }",
        "            else if cp > end { std::cmp::Ordering::Less }",
        "            else { std::cmp::Ordering::Equal }",
        "        })",
        "        .is_ok()",
        "}",
        "",
        "/// Indic Conjunct Break property value (UAX #44 derived).  Default is",
        "/// `None`.  Drives the GB9c rule \"don't break inside consonant clusters",
        "/// joined by a virama-style linker\" — essential for Devanagari, Bengali,",
        "/// Gujarati, Malayalam, Oriya, etc.",
        "#[derive(Copy, Clone, PartialEq, Eq, Debug)]",
        "#[repr(u8)]",
        "pub enum InCB {",
        "    None = 0,",
        "    Linker = 1,",
        "    Consonant = 2,",
        "    Extend = 3,",
        "}",
        "",
        "pub const IN_CB_RANGES: &[(u32, u32, InCB)] = &[",
    ]
    for start, end, value in incb_ranges:
        lines.append(f"    (0x{start:X}, 0x{end:X}, {INCB_ENUM_MAP[value]}),")
    lines += [
        "];",
        "",
        "/// Look up the InCB value for `cp`.  Returns `InCB::None` for any",
        "/// codepoint not explicitly listed.",
        "pub fn incb(cp: u32) -> InCB {",
        "    match IN_CB_RANGES.binary_search_by(|&(start, end, _)| {",
        "        if cp < start { std::cmp::Ordering::Greater }",
        "        else if cp > end { std::cmp::Ordering::Less }",
        "        else { std::cmp::Ordering::Equal }",
        "    }) {",
        "        Ok(i) => IN_CB_RANGES[i].2,",
        "        Err(_) => InCB::None,",
        "    }",
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::*;",
        "    #[test]",
        "    fn known_gbp_lookups() {",
        "        assert_eq!(gbp(b'\\r' as u32), GBP::CR);",
        "        assert_eq!(gbp(b'\\n' as u32), GBP::LF);",
        "        assert_eq!(gbp(0x200D),       GBP::ZWJ);",
        "        assert_eq!(gbp(0x1F1E6),      GBP::RegionalIndicator);",
        "        assert_eq!(gbp(0x1100),       GBP::L);",
        "        assert_eq!(gbp(0xAC00),       GBP::LV);",
        "        assert_eq!(gbp(0xAC01),       GBP::LVT);",
        "        assert_eq!(gbp(b'A' as u32),  GBP::Other);",
        "    }",
        "    #[test]",
        "    fn known_extp_lookups() {",
        "        assert!(is_extended_pictographic(0x1F468));",
        "        assert!(is_extended_pictographic(0x2764));",
        "        assert!(!is_extended_pictographic(b'A' as u32));",
        "    }",
        "    #[test]",
        "    fn known_incb_lookups() {",
        "        assert_eq!(incb(0x094D), InCB::Linker);",
        "        assert_eq!(incb(0x0915), InCB::Consonant);",
        "        assert_eq!(incb(b'A' as u32), InCB::None);",
        "    }",
        "    #[test]",
        "    fn ranges_sorted_disjoint() {",
        "        let mut prev = 0u32;",
        "        for (i, &(s, e, _)) in GRAPHEME_BREAK_PROPERTY.iter().enumerate() {",
        "            assert!(s <= e);",
        "            if i > 0 { assert!(prev < s, \"GBP range[{}] overlaps\", i); }",
        "            prev = e;",
        "        }",
        "        let mut prev = 0u32;",
        "        for (i, &(s, e, _)) in IN_CB_RANGES.iter().enumerate() {",
        "            assert!(s <= e);",
        "            if i > 0 { assert!(prev < s, \"InCB range[{}] overlaps\", i); }",
        "            prev = e;",
        "        }",
        "    }",
        "}",
    ]
    out.write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gbp", required=True, type=Path)
    ap.add_argument("--emd", required=True, type=Path)
    ap.add_argument("--dcp", required=True, type=Path)
    ap.add_argument("--out-emoji", required=True, type=Path)
    ap.add_argument("--out-unicode", required=True, type=Path)
    args = ap.parse_args()
    emit_emoji_presentation(args.emd, args.out_emoji)
    emit_unicode_data(args.gbp, args.emd, args.dcp, args.out_unicode)


if __name__ == "__main__":
    sys.exit(main() or 0)
