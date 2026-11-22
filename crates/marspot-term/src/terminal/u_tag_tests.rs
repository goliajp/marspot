use super::Terminal;

/// A pane whose plugin has said its program prints markup it does
/// not render.  That declaration is the only thing that turns this
/// on — see `MsgType::PaneRenderMarkup`.
fn declared(cols: u16, rows: u16) -> Terminal {
    let mut t = Terminal::new(cols, rows);
    t.set_render_markup(true);
    t
}

fn row(t: &Terminal, r: u16) -> String {
    (0..t.grid().cols())
        .map(|c| t.grid().cell(c, r).ch)
        .collect()
}
fn under(t: &Terminal, c: u16) -> bool {
    t.grid().cell(c, 0).attrs.underline
}

/// A matched pair styles what it wraps and nothing else.
#[test]
fn a_matched_pair_underlines_what_it_wraps() {
    let mut t = declared(20, 2);
    t.feed(b"a<u>bc</u>d");
    assert_eq!(
        row(&t, 0).trim_end(),
        "abcd",
        "the tags are styling, not text"
    );
    assert!(!under(&t, 0), "before");
    assert!(under(&t, 1) && under(&t, 2), "inside");
    assert!(!under(&t, 3), "after");
}

/// An UNMATCHED opening tag styles nothing.  It is printed, and so
/// is everything after it — text that merely mentions the tag is
/// most of any conversation about this feature, and underlining
/// the rest of the pane on account of one was the regression this
/// pins (2026-09-06).
#[test]
fn an_unmatched_open_tag_is_just_text() {
    let mut t = declared(40, 2);
    t.feed(b"see <u> in the docs");
    t.feed(b"\r\n");
    assert_eq!(row(&t, 0).trim_end(), "see <u> in the docs");
    for c in 0..20 {
        assert!(!under(&t, c), "column {c} must not be underlined");
    }
}

/// The tags take no width once matched: they are markup, and a
/// terminal that left seven blank cells behind would push the
/// rest of the line out of the place the program laid it out for.
#[test]
fn matched_tags_occupy_no_cells() {
    let mut t = declared(20, 2);
    t.feed(b"<u>ab</u>!");
    assert_eq!(row(&t, 0).trim_end(), "ab!");
    assert_eq!(t.grid().cursor().0, 3, "cursor sits right after the text");
    for c in 3..20 {
        assert_eq!(t.grid().cell(c, 0).ch, ' ', "column {c} must be untouched");
    }
}

/// A closing tag with nothing open is text too.
#[test]
fn a_stray_closing_tag_is_just_text() {
    let mut t = declared(30, 2);
    t.feed(b"a</u>b");
    assert_eq!(row(&t, 0).trim_end(), "a</u>b");
    assert!(!under(&t, 0) && !under(&t, 5));
}

/// A span cannot cross a line: the newline ends it, and the tag
/// comes back as text.
#[test]
fn a_span_broken_by_a_newline_is_not_styled() {
    let mut t = declared(30, 3);
    t.feed(b"x<u>abc\r\ndef</u>");
    assert_eq!(row(&t, 0).trim_end(), "x<u>abc");
    for c in 0..7 {
        assert!(!under(&t, c), "column {c}");
    }
}

/// A tag split across two PTY reads is still one tag: the state is
/// the terminal's, not the chunk's.
#[test]
fn a_tag_split_across_reads_still_counts() {
    let mut t = declared(20, 2);
    t.feed(b"a<");
    t.feed(b"u>b");
    t.feed(b"</u>c");
    assert_eq!(row(&t, 0).trim_end(), "abc");
    assert!(under(&t, 1) && !under(&t, 2));
}

/// Text that merely starts with `<` comes out untouched.
#[test]
fn text_that_is_not_a_tag_is_printed_verbatim() {
    for text in ["a<b>c", "a<ub>c", "a</x>c", "a<uu>c", "if a<b then"] {
        let mut t = declared(30, 2);
        t.feed(text.as_bytes());
        assert_eq!(row(&t, 0).trim_end(), text, "{text:?} was altered");
    }
}

/// A dangling `<` at the end of a line must still appear.
#[test]
fn a_dangling_open_bracket_is_not_eaten() {
    let mut t = declared(20, 2);
    t.feed(b"a<");
    t.feed(b"\r\n");
    assert!(row(&t, 0).starts_with("a<"), "got {:?}", row(&t, 0));
}

/// An escape sequence inside a span ends it — the span's text is
/// printed with its tag, which is what the pane showed before the
/// feature existed.
#[test]
fn an_escape_sequence_inside_a_span_falls_back_to_text() {
    let mut t = declared(40, 2);
    t.feed(b"<u>ab\x1b[31mcd</u>");
    assert!(row(&t, 0).starts_with("<u>ab"), "got {:?}", row(&t, 0));
    assert!(!under(&t, 3));
}

/// An UNDECLARED pane leaves the characters alone.  This is the
/// default, and it is what makes the feature safe to have at all:
/// the panes where people discuss markup are not the panes a
/// plugin declared.
#[test]
fn an_undeclared_pane_shows_the_tags() {
    let mut t = Terminal::new(30, 2);
    t.feed(b"a<u>bc</u>d");
    assert_eq!(row(&t, 0).trim_end(), "a<u>bc</u>d");
    assert!(!under(&t, 1));
}
