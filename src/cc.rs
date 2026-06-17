//! Claudecode-specific selection / copy post-processing.
//!
//! claudecode (the TUI we wrap inside marspot) renders to a fixed inner
//! width: long lines get hard-wrapped with `\n` + (sometimes) a 2-space
//! hanging indent.  The grid sees real newlines, not DECAWM soft wraps,
//! so `grid_selection_text` cannot fold them — by the time we have
//! string text, the original column boundary is gone.
//!
//! The fix is heuristic and scoped: only invoked when the focused pane
//! is recognised as a cc pane (non-empty `pane_badges` entry).  Other
//! TUIs / shells stay on the unchanged path.
//!
//! Heuristic v1 — process paragraph-by-paragraph (paragraph = group of
//! consecutive non-empty lines, separated by blank lines):
//!
//!   - within a paragraph, fold line N → line N+1 into one logical line
//!     unless either side is a "structural" break (bullet, code fence,
//!     sentence-final punctuation)
//!   - the join uses a single ASCII space when both sides are ASCII
//!     word-class, NO space when both sides are CJK / wide (CJK soft
//!     wrap doesn't introduce whitespace), and a single space otherwise
//!   - blank lines (paragraph separators) are preserved verbatim
//!   - hanging-indent continuation lines (lead with ≥ 2 spaces while
//!     the first line of the paragraph doesn't) have their leading
//!     indent stripped before joining
//!
//! v1 is conservative: when in doubt, keep the newline.  Better to
//! under-fold and let the user retry than to over-fold a list /
//! bullet block into a wall of text.

/// Rejoin wrapped paragraphs in cc-style output.  Pure string→string.
pub fn rejoin_wrapped_paragraphs(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let trailing_nl = text.ends_with('\n');
    let lines: Vec<&str> = text.split('\n').collect();

    // Walk the line list once, emitting segments (paragraphs are rejoined
    // into a single segment, blank lines stay as empty segments).  The
    // final `join("\n")` reconstructs the original blank/paragraph
    // structure exactly.
    let mut segments: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            segments.push(String::new());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && !lines[i].trim().is_empty() {
            i += 1;
        }
        segments.push(rejoin_paragraph(&lines[start..i]));
    }

    let mut out = segments.join("\n");
    if trailing_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Fold a single paragraph (≥ 1 non-blank lines, no blanks inside)
/// into one logical line — except where the line break is "structural"
/// and must be preserved (bullets, code fences, sentence-final punct
/// when the next line starts capitalised or with a bullet, etc.).
fn rejoin_paragraph(lines: &[&str]) -> String {
    if lines.len() == 1 {
        return lines[0].trim_end().to_string();
    }

    // Paragraph contains a code fence — leave it entirely alone.
    // Folding ``` and the code inside it produces garbage; the cost
    // of erring this way is at most an unfolded prose paragraph that
    // happens to contain a literal ``` line, which is rare.
    if lines.iter().any(|l| l.trim_start().starts_with("```")) {
        return lines
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut out = String::with_capacity(lines.iter().map(|l| l.len()).sum::<usize>() + lines.len());
    out.push_str(lines[0].trim_end());

    for j in 1..lines.len() {
        let prev_tail = last_non_space_char(&out);
        let next = lines[j];
        let next_trimmed_start = next.trim_start();
        let next_head = next_trimmed_start.chars().next();

        if should_keep_break(prev_tail, next_trimmed_start) {
            out.push('\n');
            out.push_str(next.trim_end());
            continue;
        }

        // Pick the join glue.  Both-side wide → empty (CJK soft wrap).
        // Otherwise single space, collapsing any existing trailing
        // / leading whitespace.
        let glue = match (prev_tail, next_head) {
            (Some(p), Some(n)) if is_wide(p) && is_wide(n) => "",
            _ => " ",
        };
        // Drop trailing whitespace from `out` so glue is canonical.
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        out.push_str(glue);
        out.push_str(next_trimmed_start.trim_end());
    }

    out
}

/// Decide whether the break between lines should survive folding.
/// Structural breaks (bullets, code-fence rows, sentence boundary
/// followed by a capitalised new sentence) are preserved.
fn should_keep_break(prev_tail: Option<char>, next_trimmed: &str) -> bool {
    // Empty continuation is technically blank; we already filter
    // blank lines out before calling this, but guard anyway.
    if next_trimmed.is_empty() {
        return true;
    }

    // Next line is a bullet / list marker → keep break.
    if starts_with_bullet(next_trimmed) {
        return true;
    }

    // Next line is a code fence (```), keep.
    if next_trimmed.starts_with("```") {
        return true;
    }

    // Sentence-final punctuation followed by capitalised start of a
    // new sentence → keep.  Comma / dash / hyphen do NOT count.
    let next_first = next_trimmed.chars().next().unwrap();
    if let Some(p) = prev_tail {
        if matches!(p, '.' | '!' | '?' | '。' | '！' | '？')
            && (next_first.is_uppercase() || is_wide(next_first))
        {
            return true;
        }
    }

    false
}

fn starts_with_bullet(s: &str) -> bool {
    // "- foo", "* foo", "• foo", "● foo" (cc's bullet glyph),
    // "1. foo", "2) foo", "> quoted"
    let s = s;
    if let Some(first) = s.chars().next() {
        if matches!(first, '-' | '*' | '•' | '●' | '◆' | '◇' | '▪' | '▫' | '>') {
            // Require the marker to be followed by whitespace, else
            // it's likely a leading hyphen in a hyphenated word.
            return s.chars().nth(1).map(|c| c.is_whitespace()).unwrap_or(false);
        }
        if first.is_ascii_digit() {
            // "<digits><.|)><space>..."
            let mut it = s.chars();
            let mut saw_digit = false;
            for c in it.by_ref() {
                if c.is_ascii_digit() {
                    saw_digit = true;
                    continue;
                }
                if saw_digit && (c == '.' || c == ')') {
                    return it.next().map(|c| c.is_whitespace()).unwrap_or(false);
                }
                break;
            }
        }
    }
    false
}

fn last_non_space_char(s: &str) -> Option<char> {
    s.chars().rev().find(|c| !c.is_whitespace())
}

/// Treat East-Asian wide characters as "no soft-space" candidates.
/// Conservative range coverage — extend as need arises.
fn is_wide(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
          0x1100..=0x115F  // Hangul Jamo
        | 0x2E80..=0x303E  // CJK Radicals / Symbols
        | 0x3041..=0x33FF  // Hiragana / Katakana / Bopomofo / CJK Symbols
        | 0x3400..=0x4DBF  // CJK Ext A
        | 0x4E00..=0x9FFF  // CJK Unified
        | 0xA000..=0xA4CF  // Yi
        | 0xAC00..=0xD7A3  // Hangul Syllables
        | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
        | 0xFE30..=0xFE4F  // CJK Compatibility Forms
        | 0xFF00..=0xFF60  // Fullwidth ASCII
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_soft_wrap_rejoins_with_space() {
        let src = "Hello world this is a long\nsentence that wraps.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Hello world this is a long sentence that wraps."
        );
    }

    #[test]
    fn cjk_soft_wrap_rejoins_without_space() {
        let src = "这是一段中文内容会自动\n换行不应该带空格。";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "这是一段中文内容会自动换行不应该带空格。"
        );
    }

    #[test]
    fn paragraph_break_preserved() {
        let src = "First paragraph line one\nends mid sentence.\n\nSecond paragraph.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "First paragraph line one ends mid sentence.\n\nSecond paragraph."
        );
    }

    #[test]
    fn bullet_list_unchanged() {
        let src = "Items:\n- one\n- two\n- three";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Items:\n- one\n- two\n- three"
        );
    }

    #[test]
    fn numbered_list_unchanged() {
        let src = "Steps:\n1. open editor\n2. type something\n3. save";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Steps:\n1. open editor\n2. type something\n3. save"
        );
    }

    #[test]
    fn cc_bullet_glyph_unchanged() {
        // claudecode uses `●` as its assistant turn marker.
        let src = "● First reply line that wraps\n  onto continuation.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "● First reply line that wraps onto continuation."
        );
    }

    #[test]
    fn hanging_indent_continuation_stripped() {
        let src = "Line that wraps with hanging\n  indent on continuation.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Line that wraps with hanging indent on continuation."
        );
    }

    #[test]
    fn code_fence_preserved() {
        let src = "Here is code:\n```\nfn main() {}\n```\nDone.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Here is code:\n```\nfn main() {}\n```\nDone."
        );
    }

    #[test]
    fn sentence_end_then_capital_keeps_break() {
        // "Foo.\nBar starts new sentence" — keep the break since
        // intra-paragraph sentence joins read awkwardly.
        let src = "First sentence.\nSecond Sentence here.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "First sentence.\nSecond Sentence here."
        );
    }

    #[test]
    fn sentence_end_then_lowercase_joins() {
        // ". the" still rejoins because the user just selected text
        // that happened to break at a sentence — but the lowercase
        // start signals the break was a wrap, not a deliberate one.
        let src = "First sentence.\nthe continuation lowercase.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "First sentence. the continuation lowercase."
        );
    }

    #[test]
    fn comma_wrap_rejoins() {
        let src = "Cargo build succeeded,\nrunning tests now.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Cargo build succeeded, running tests now."
        );
    }

    #[test]
    fn trailing_newline_preserved() {
        let src = "Hello world\nthis wraps.\n";
        let got = rejoin_wrapped_paragraphs(src);
        assert!(got.ends_with('\n'));
        assert_eq!(got, "Hello world this wraps.\n");
    }

    #[test]
    fn single_line_passthrough() {
        assert_eq!(rejoin_wrapped_paragraphs("just one line"), "just one line");
    }

    #[test]
    fn empty_passthrough() {
        assert_eq!(rejoin_wrapped_paragraphs(""), "");
    }

    #[test]
    fn cjk_to_cjk_at_boundary_no_space() {
        // Both sides of the join are wide → no soft space.  Pure CJK
        // soft wrap should rejoin tightly.
        let src = "Hello world中\n文继续.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "Hello world中文继续."
        );
    }

    #[test]
    fn ascii_to_cjk_boundary_uses_space() {
        // Mixed boundary (ASCII end → CJK start, or vice versa) →
        // default to a single space.  Documenting current behaviour
        // so future tweaks are visible.
        let src = "ends in ascii\n中文继续.";
        assert_eq!(
            rejoin_wrapped_paragraphs(src),
            "ends in ascii 中文继续."
        );
    }
}
