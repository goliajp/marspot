//! Locating the composer (input box) of a TUI so its rows are left unscanned.

use crate::CellSource;

/// Locate claudecode's composer in the visible grid, as an inclusive
/// `(top_row, bottom_row)` range to exclude from link scanning.
///
/// **This used to look for a rounded box** (`╭…╮` / `╰…╯`) and return
/// `None` when it found none.  That has now failed twice, for the
/// same reason both times: the thing it keyed on was *decoration*,
/// and claudecode redecorates.  v2.1.212 dropped the box, which made
/// the bottom-most box the welcome banner (fixed then by requiring
/// the caret or a bottom-hugging position).  The current version
/// draws no box at all — two grey `─` rules with the prompt between
/// them — so corner detection finds nothing, the exemption never
/// fires, and the composer gets scanned like body text: a path
/// underlines itself while you are still typing it.
///
/// So the anchor is the **caret**.  A caret is protocol, not
/// styling; no redesign can remove it, and in a claudecode pane it
/// lives in the composer.  The composer is bottom-anchored in every
/// version we have seen, so:
///
///   - the caret must sit in the bottom third of the view — higher
///     than that and it is somewhere in the output, not the input,
///     and exempting downward from it would swallow real text;
///   - the range runs from the caret's row to the last row: below
///     the composer there is only claudecode's own footer;
///   - it widens *upward* to the nearest separator row (a run of box
///     horizontals, which covers both the `─` rules of today and the
///     `╭` of the old box) so a multi-line prompt is covered whole,
///     not just the line the caret happens to be on.  The search is
///     bounded, and finding nothing simply means the caret's row
///     alone — under-exempting is recoverable, swallowing output is
///     not.
///
/// The legacy corner scan is kept as a fallback for the one case the
/// caret cannot speak for: a scrolled-back view, where the caret is
/// outside the window entirely.
pub(crate) fn find_input_box_rows<S: CellSource>(
    src: &S,
    rows: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    let (_, caret_row) = src.cursor();
    if caret_row < rows {
        // "Near the bottom", expressed as a distance rather than a
        // fraction: the composer is at most `MAX_COMPOSER_ROWS` tall,
        // and never more than half the view — a fraction alone
        // degenerates on short panes, where a third of ten rows is
        // three and the composer is half the screen.
        let reach = MAX_COMPOSER_ROWS.min(rows / 2);
        if caret_row + reach >= rows {
            let mut r = caret_row;
            let mut walked = 0u16;
            while r > 0 && walked < MAX_COMPOSER_ROWS {
                r -= 1;
                walked += 1;
                if is_separator_row(src, r, cols) {
                    return Some((r, rows - 1));
                }
            }
            // No rule above the caret: this is the chromeless
            // composer (claudecode v2.1.212 ships one), so the
            // caret's own row is the exemption — UNLESS that row is
            // a soft-wrap continuation.  A composer's first row is
            // never one: it is a prompt the app drew, not the tail
            // of a line the terminal wrapped.
            //
            // Exempting it anyway truncated links in the 2026-09-04
            // report.  A path wrapped across the last two rows had
            // its continuation swallowed by the phantom box, so the
            // scan saw only `…/notes/provenance-probe` — not a real
            // path — and backed off to the longest prefix that was,
            // `…/spg/`.  The link stopped four segments short of the
            // file it pointed at.
            if !src.is_soft_wrap_continuation(caret_row)
                && looks_like_prompt_row(src, caret_row, cols)
            {
                return Some((caret_row, rows - 1));
            }
        }
    }
    find_rounded_box_rows(src, rows, cols)
}

/// How far above the caret to look for the composer's opening rule.
/// Generous enough for a multi-line prompt, short enough that a
/// missing separator cannot eat a screenful of output.
const MAX_COMPOSER_ROWS: u16 = 12;

/// Does this row look like a prompt the app drew, rather than a line
/// of output the caret happens to be resting on?
///
/// This is the whole basis for exempting a chromeless composer.  The
/// caret sits at the end of the last output line for most of a pane's
/// life, so "the caret is here" identifies nothing — taking it as the
/// composer blanked the links on ordinary output (2026-09-04).  What
/// actually marks the composer is its prompt glyph.
///
/// Deliberately asymmetric: guessing wrong here costs one spurious
/// link inside a composer, while guessing wrong the other way silently
/// drops every link near the bottom of the pane.
fn looks_like_prompt_row<S: CellSource>(src: &S, row: u16, cols: u16) -> bool {
    for c in 0..cols {
        match src.char_at(c, row) {
            ' ' | '\0' => continue,
            '❯' | '>' | '›' | '⟩' | '»' | '$' | '%' => return true,
            _ => return false,
        }
    }
    false
}

/// A row that is drawn rule, not text: box horizontals and corners
/// with nothing else on it.  Both claudecode chromes qualify — the
/// old `╭────╮` and the current bare `────`.
fn is_separator_row<S: CellSource>(src: &S, row: u16, cols: u16) -> bool {
    let mut rule = 0u16;
    let mut other = 0u16;
    for c in 0..cols {
        match src.char_at(c, row) {
            ' ' | '\0' => {}
            '─' | '━' | '═' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '│' | '├' | '┤'
            | '┬' | '┴' | '┼' => rule += 1,
            _ => other += 1,
        }
    }
    // Half the width of rule glyphs and nothing else: the composer's
    // divider spans most of the pane, while a table row of the same
    // glyphs carries text between them.
    other == 0 && rule >= cols / 2
}

/// The pre-2026-08 detection, kept for the scrolled-back case where
/// the caret is not in view.  Returns the bottom-most rounded box.
fn find_rounded_box_rows<S: CellSource>(
    src: &S,
    rows: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    let mut bottom: Option<u16> = None;
    for r in (0..rows).rev() {
        if row_contains_any(src, r, cols, &['╰', '╯']) {
            bottom = Some(r);
            break;
        }
    }
    let bottom = bottom?;
    if bottom == 0 {
        return None;
    }
    let mut top: Option<u16> = None;
    for r in (0..bottom).rev() {
        if row_contains_any(src, r, cols, &['╭', '╮']) {
            top = Some(r);
            break;
        }
    }
    let top = top?;
    // 2026-07-18 field regression — the bottom-most rounded box is
    // only the COMPOSER when it looks like an active input area:
    // it either contains the cursor row (the user's caret lives in
    // the composer) or hugs the bottom of the viewport.  claudecode
    // v2.1.212 dropped the composer's box entirely, which made the
    // bottom-most box the WELCOME BANNER at the top of the screen —
    // exempting it swallowed the banner's email/path links.
    let (_, cursor_row) = src.cursor();
    let has_cursor = cursor_row >= top && cursor_row <= bottom;
    let hugs_bottom = bottom + 4 >= rows;
    if !has_cursor && !hugs_bottom {
        return None;
    }
    Some((top, bottom))
}

fn row_contains_any<S: CellSource>(
    src: &S,
    row: u16,
    cols: u16,
    targets: &[char],
) -> bool {
    for c in 0..cols {
        let ch = src.char_at(c, row);
        if targets.contains(&ch) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::StrSource;
    use crate::{ScanOpts, scan_visible_links};

    /// claudecode's composer as it is drawn **today** — two grey
    /// rules with the prompt between them, no rounded corners
    /// anywhere.  Captured off the wire from a live pane
    /// (2026-08-14): `…/effort ␛[1B ─────… ␛[1B ❯ ␛[6G␛[K ␛[1B
    /// ─────… ⏵⏵ bypass permissions on`.
    ///
    /// The path being typed must not underline itself, and the path
    /// in the output *above* the composer must still be a link —
    /// exempting the composer is not permission to stop scanning.
    #[test]
    fn the_composer_is_exempt_even_when_it_has_no_box() {
        // Real paths on both sides: a span only survives the scan if
        // it exists on disk, so a fixture of invented names would
        // pass whether or not the exemption worked.
        let rule: String = "─".repeat(40);
        let rows = [
            "wrote /etc/hosts just now",
            "",
            "  Ran 1 shell command",
            "",
            "some more output",
            rule.as_str(),
            "❯ /usr/bin",
            rule.as_str(),
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        ];
        let mut src = StrSource::new(&rows, 46);
        src.cursor = (10, 6); // caret in the prompt row
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(
            links.iter().all(|l| l.row != 2),
            "the composer must not be scanned: {links:?}",
        );
        assert!(
            links.iter().any(|l| l.row == 0 && l.text.contains("/etc/hosts")),
            "output above the composer is still linkable: {links:?}",
        );
    }

    /// Also found while investigating the 2026-09-04 report, and also
    /// not its cause: with a DECAWM soft wrap (rather than the
    /// claudecode hard wrap that report turned out to involve), a
    /// caret coming to rest on the continuation — the
    /// tail of a path the terminal had wrapped — and the chromeless-
    /// composer branch claimed it, so the continuation never joined
    /// the row above it.  The scan then saw a prefix that is not a
    /// real path and backed off to the longest one that is.
    ///
    /// A composer's first row is drawn by the app; it is never the
    /// tail of a wrapped line.  That is the whole distinction.
    #[test]
    fn a_caret_resting_on_a_wrapped_tail_is_not_a_composer() {
        // `/etc/hosts` exists everywhere this builds.
        let rows = ["see /etc/hos", "ts here"];
        let mut src = StrSource::new(&rows, 12);
        src.soft_wrapped = vec![false, true];
        src.cursor = (7, 1); // caret parked on the continuation
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(
            links.iter().any(|l| l.text == "/etc/hosts"),
            "the wrapped tail was swallowed by a phantom composer: {links:?}",
        );
    }

    /// A separate defect found while investigating the 2026-09-04
    /// report — not its cause (that was the seam above), but real:
    /// paths that did NOT wrap lost their links entirely near the
    /// bottom of the pane.
    ///
    /// The caret comes to rest at the end of the last line of output.
    /// Treating that as the chromeless composer exempts the very line
    /// the user is looking at — not a truncated link, no link at all.
    #[test]
    fn a_caret_at_the_end_of_output_does_not_blank_its_own_row() {
        let rows = ["first line", "see /etc/hosts here"];
        let mut src = StrSource::new(&rows, 24);
        src.cursor = (19, 1); // caret where output left it
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(
            links.iter().any(|l| l.text == "/etc/hosts"),
            "output row blanked by a phantom composer: {links:?}",
        );
    }

    /// The shape claudecode shipped in v2.1.212 — no box, no rules,
    /// just the prompt line.  There is nothing above the caret to
    /// widen to, so the caret's own row is the exemption, and the
    /// bounded upward walk must not swallow the output above it.
    #[test]
    fn a_composer_with_no_chrome_at_all_still_exempts_its_own_row() {
        let rows = [
            "see /etc/hosts for details",
            "and /usr/share too",
            "❯ /usr/bin",
        ];
        let mut src = StrSource::new(&rows, 40);
        src.cursor = (10, 2);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(links.iter().all(|l| l.row != 2), "caret row exempt: {links:?}");
        assert!(
            links.iter().filter(|l| l.row == 0 || l.row == 1).count() >= 2,
            "both output rows keep their links: {links:?}",
        );
    }

    /// A caret parked up in the output — which happens mid-repaint —
    /// must not exempt everything below it.  This is the failure the
    /// bottom-third guard exists to prevent, and it is worse than the
    /// bug being fixed: it would silently drop links from real text.
    #[test]
    fn a_caret_in_the_output_does_not_blank_the_rows_below_it() {
        let rows = [
            "line one",
            "see /etc/hosts here",
            "see /usr/bin here",
            "see /usr/lib here",
            "see /var/log here",
            "see /usr/share here",
        ];
        let mut src = StrSource::new(&rows, 30);
        src.cursor = (0, 1); // top of the view, nowhere near a composer
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        for r in 1..=5u16 {
            assert!(
                links.iter().any(|l| l.row == r),
                "row {r} lost its link to a bogus exemption: {links:?}",
            );
        }
    }

    /// A separator row is drawn rule and nothing else.  A table row
    /// built from the same glyphs carries text between them and must
    /// not be mistaken for one, or the upward walk would stop early
    /// and leave part of a multi-line prompt scanned.
    #[test]
    fn a_table_row_is_not_a_separator() {
        let src = StrSource::new(&["│ name │ size │", "──────────────"], 15);
        assert!(!is_separator_row(&src, 0, 15), "text between rules is a table");
        assert!(is_separator_row(&src, 1, 15), "a bare rule is a separator");
    }

    /// Trait-path smoke: soft-wrap merge + tui_mode input-box
    /// exemption through `scan_visible_links` over a plain
    /// `StrSource` — the exact surface an external consumer sees.
    #[test]
    fn cellsource_scan_merges_soft_wrap_and_exempts_composer() {
        let mut src = StrSource::new(
            &[
                "see https://example.c",
                "om/long/path now ok  ",
                "                     ",
                "╭───────────────────╮",
                "│ > https://foo.com │",
                "╰───────────────────╯",
            ],
            21,
        );
        src.soft_wrapped[1] = true;
        src.cursor = (4, 4);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert_eq!(links.len(), 2, "{links:?}"); // one URL fanned over 2 rows
        assert!(links.iter().all(|l| l.text == "https://example.com/long/path"));
        assert_eq!((links[0].row, links[1].row), (0, 1));
        // Without tui_mode the composer URL is scanned too.
        let all = scan_visible_links(&src, ScanOpts::default());
        assert!(all.iter().any(|l| l.text == "https://foo.com"), "{all:?}");
    }
}
