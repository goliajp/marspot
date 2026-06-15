#!/usr/bin/env bash
# Regenerate crates/marspot-term/src/emoji_presentation.rs from the
# Unicode emoji-data.txt (UTS #51).  Run when a new Unicode version
# ships and we want to pick up newly-assigned emoji.  Hand-editing
# the generated file is discouraged — anything you change will be
# clobbered on next regen.
#
# Usage:
#   bin/regen-emoji-presentation.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/marspot-term/src/emoji_presentation.rs"
URL="https://www.unicode.org/Public/UCD/latest/ucd/emoji/emoji-data.txt"

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "==> fetching $URL"
curl -fsSL -o "$TMP" "$URL"

# Capture the version stamp from the file header so the doc comment
# stays honest about what we generated against.
VERSION_LINE="$(grep -m1 '^# Version:' "$TMP" | sed 's/^# *//')"
DATE_LINE="$(grep -m1 '^# Date:' "$TMP" | sed 's/^# *//')"

echo "==> writing $OUT  ($VERSION_LINE; $DATE_LINE)"
awk -v ver="$VERSION_LINE" -v date="$DATE_LINE" '
BEGIN {
    print "//! Generated from the Unicode emoji-data.txt (UTS #51) — every"
    print "//! codepoint with `Emoji_Presentation=Yes`.  These are the code"
    print "//! points that default to emoji (color, em-box-wide) presentation,"
    print "//! the canonical definition of \"is this character an emoji\" for"
    print "//! the terminal-width purpose.  Characters with Emoji=Yes but"
    print "//! Emoji_Presentation=No (e.g. ⚠ U+26A0, ☝ U+261D) default to TEXT"
    print "//! presentation — they become emoji only when followed by VS16"
    print "//! (U+FE0F), which the parser handles separately."
    print "//!"
    print "//! Source: https://www.unicode.org/Public/UCD/latest/ucd/emoji/emoji-data.txt"
    print "//! " ver
    print "//! " date
    print "//!"
    print "//! Regeneration: see bin/regen-emoji-presentation.sh."
    print ""
    print "/// Sorted, non-overlapping inclusive ranges `(start, end)` of"
    print "/// codepoints with `Emoji_Presentation=Yes`.  Binary search via"
    print "/// [`has_emoji_presentation`] is the only intended access path."
    print "pub const EMOJI_PRESENTATION_RANGES: &[(u32, u32)] = &["
}
/; Emoji_Presentation /{
    sub(/[ \t]+$/, "", $1)
    n = split($1, parts, /\.\./)
    if (n == 2) {
        printf "    (0x%s, 0x%s),\n", parts[1], parts[2]
    } else {
        printf "    (0x%s, 0x%s),\n", $1, $1
    }
}
END {
    print "];"
    print ""
    print "/// `true` iff `cp` is an Emoji_Presentation=Yes codepoint per UTS #51."
    print "/// Single-pass binary search over `EMOJI_PRESENTATION_RANGES`."
    print "pub fn has_emoji_presentation(cp: u32) -> bool {"
    print "    EMOJI_PRESENTATION_RANGES"
    print "        .binary_search_by(|&(start, end)| {"
    print "            if cp < start {"
    print "                std::cmp::Ordering::Greater"
    print "            } else if cp > end {"
    print "                std::cmp::Ordering::Less"
    print "            } else {"
    print "                std::cmp::Ordering::Equal"
    print "            }"
    print "        })"
    print "        .is_ok()"
    print "}"
    print ""
    print "#[cfg(test)]"
    print "mod tests {"
    print "    use super::*;"
    print "    #[test]"
    print "    fn known_presentation_emoji_match_spec() {"
    print "        // User-reported via 2026-06-15 emoji rendering complaint."
    print "        assert!(has_emoji_presentation(0x2705), \"✅\");"
    print "        assert!(has_emoji_presentation(0x274C), \"❌\");"
    print "        assert!(has_emoji_presentation(0x2B50), \"⭐\");"
    print "        assert!(has_emoji_presentation(0x1F33F), \"🌿\");"
    print "        // Text-default per the spec — should NOT match."
    print "        assert!(!has_emoji_presentation(0x26A0), \"⚠ defaults to text\");"
    print "        assert!(!has_emoji_presentation(0x261D), \"☝ defaults to text\");"
    print "        // Plain ASCII never matches."
    print "        assert!(!has_emoji_presentation(b\"A\"[0] as u32));"
    print "        assert!(!has_emoji_presentation(0x4E2D), \"中 (CJK Unified Ideograph)\");"
    print "    }"
    print "    #[test]"
    print "    fn ranges_are_sorted_and_disjoint() {"
    print "        let mut prev_end = 0u32;"
    print "        for (i, &(s, e)) in EMOJI_PRESENTATION_RANGES.iter().enumerate() {"
    print "            assert!(s <= e, \"range[{}] start > end\", i);"
    print "            if i > 0 {"
    print "                assert!(prev_end < s, \"range[{}] overlaps/adjoins predecessor\", i);"
    print "            }"
    print "            prev_end = e;"
    print "        }"
    print "    }"
    print "}"
}
' "$TMP" > "$OUT"

n=$(grep -c '^    (0x' "$OUT")
echo "==> regenerated $n ranges"
