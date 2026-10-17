//! The hard-wrap join, end to end: which row pairs are one token cut in two.

use super::*;

/// codex wraps flush at column 0, and its URLs were cut at the
/// wrap.
///
/// The three rows are copied off pane 435 at 73 columns
/// (2026-09-26): a 401 body whose two URLs each break mid-token,
/// once right after the scheme and once inside the path.  What the
/// user saw: the first URL not underlined at all, and the second
/// underlined as `https://chatgpt.com/backend-` — a URL that
/// exists and goes somewhere else.
#[test]
fn a_url_wrapped_flush_at_column_zero_is_still_one_url() {
    let rows = [
        "**************fvMA. You can find your API key at https://",
        "platform.openai.com/account/api-keys., url: https://chatgpt.com/backend-",
        "api/codex/responses, cf-ray: a40db52ee9100dce-NRT, request id: 3acb2d64-",
        "504d-49d5-9030-a179087f4bce",
        "",
    ];
    let mut src = StrSource::new(&rows, 73);
    src.cursor = (0, 4);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    assert!(
        texts.contains(&"https://platform.openai.com/account/api-keys"),
        "the URL broken after its scheme must rejoin, got {texts:?}"
    );
    assert!(
        texts.contains(&"https://chatgpt.com/backend-api/codex/responses"),
        "the URL broken inside its path must rejoin, got {texts:?}"
    );
    assert!(
        !texts.contains(&"https://chatgpt.com/backend-"),
        "and must never be offered as the half that fits, got {texts:?}"
    );
}

/// A row with nothing left over is not evidence of anything.
///
/// It can end at the edge because it wrapped or because it fitted;
/// the screen cannot say which, so the flush seam keeps stopping
/// the URL.  `marspot-term`'s own `zero_indent_does_not_glue_urls`
/// watches the same line from the grid side — this is the case the
/// first cut of the forced-break test got wrong, because with no
/// room left every word looks like it did not fit.
#[test]
fn a_full_row_is_not_evidence_of_a_wrap() {
    let url = format!("https://example.com/{}", "a".repeat(10)); // 30 chars
    let rows = [url.as_str(), "and more prose", ""];
    let mut src = StrSource::new(&rows, 30);
    src.cursor = (0, 2);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec![url.as_str()], "the URL must stop at the row edge");
}

/// A break the renderer did not have to make is a real newline.
///
/// Same shape — a URL ending a row, a flush row under it opening
/// with path characters — but the word below had room on the row
/// above.  Joining those would invent a URL that is on neither
/// row, and nothing would arbitrate it.
#[test]
fn a_flush_row_that_had_room_is_not_a_wrap() {
    let rows = [
        "see https://example.com",
        "api/codex/responses is the endpoint",
        "",
    ];
    let mut src = StrSource::new(&rows, 73);
    src.cursor = (0, 2);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    assert!(
        texts.contains(&"https://example.com"),
        "the URL that is actually there, got {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("example.comapi")),
        "nothing may be glued across a break that was not forced, got {texts:?}"
    );
}

/// A path wrapped mid-token, with the break landing on an
/// ordinary character rather than on a separator.
///
/// The merge used to demand the first row end within 2 cells of
/// the pane edge, relaxing to 8 only when the row ended in `/`.
/// claudecode's `●`/`⎿` blocks wrap at a content width several
/// cells short of the pane, so where the break lands decides
/// whether the path survives — and the path does not get a say in
/// that.  Here it landed inside `…-e18-ve` / `rdict.md`: the
/// halves stayed separate, the first half named nothing that
/// exists, and the link fell back to the last directory that did
/// (`~/workspace/labs/lab36-continus/`), which is exactly what the
/// user saw underlined (2026-08-16).
///
/// Swept across the gap widths a real pane produces, because a
/// fixture at one width only proves that width.
#[test]
fn a_path_wrapped_mid_token_is_still_one_path() {
    let dir = std::env::temp_dir().join("marspot-linkify-wrap-test");
    let deep = dir.join("reports");
    if std::fs::create_dir_all(&deep).is_err() {
        return; // no temp dir: nothing to assert against
    }
    let file = deep.join("2026-08-16-e18-verdict.md");
    if std::fs::write(&file, b"x").is_err() {
        return;
    }
    let full = file.to_string_lossy().into_owned();
    // Cut the path mid-token, two characters before the end.
    let cut = full.len() - 8;
    let r0 = format!("● Write({}", &full[..cut]);
    let r1 = format!("  {})", &full[cut..]);
    let width = r0.chars().count() as u16;

    // Gaps of 0..=7 columns between the text and the pane edge.
    // Seven is what the slack buys; the case that prompted this
    // was a gap of two, which the old slack of 2 already refused
    // (it tolerated one), so the margin is real rather than
    // fitted to the one report.
    for gap in 0..=7u16 {
        let cols = width + gap;
        let mut src = StrSource::new(&[r0.as_str(), r1.as_str(), ""], cols);
        src.cursor = (0, 2);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(
            links.iter().any(|l| l.text == full),
            "gap {gap}: the two halves must rejoin into {full:?}, got {:?}",
            links.iter().map(|l| &l.text).collect::<Vec<_>>(),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// claudecode's `⎿` result block hangs its wrapped continuation at
/// FIVE cells (`  ⎿  `), one past the indent bound that was
/// measured off its prose blocks.  Every path those blocks wrapped
/// lost its tail: the link stopped at the last directory that
/// happened to exist on disk.  Reproduced from session 384's
/// bytelog, which breaks the line with `\r ESC[5C ESC[1B` —
/// geometrically a five-cell hanging indent.
#[test]
fn a_path_wrapped_into_a_five_cell_hanging_indent_still_joins() {
    // Five leading cells stand in for `  ⎿  `, so the test does not
    // also depend on that glyph's width.
    let rows = ["     /etc/ho", "     sts"];
    let src = StrSource::new(&rows, 12);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    assert!(
        links.iter().any(|l| l.text == "/etc/hosts"),
        "five-cell hanging indent did not join: {links:?}",
    );
}

/// The bound only widens for a tail that carries a separator.  A
/// prose row has no `stat` to arbitrate a wrong join, so a line
/// five cells in is still its own line — and the path that starts
/// it keeps its link instead of being glued onto the word above.
#[test]
fn a_prose_tail_does_not_swallow_the_next_line_at_five_cells() {
    let rows = ["     plain wor", "     /etc/hosts"];
    let src = StrSource::new(&rows, 15);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    assert!(
        links.iter().any(|l| l.text == "/etc/hosts"),
        "prose row was joined and swallowed the path below it: {links:?}",
    );
}

/// 2026-09-06 field report: a path wrapped by claudecode inside a
/// 73-column pane underlined only as far as `…/lab36-continus/`.
///
/// claudecode does not wrap at the pane edge — it wraps at its own
/// content width, an indent inside it, and that width differs by
/// block.  The first row stopped at column 65 of a 73-column pane,
/// eight cells short, and the flush test allowed exactly eight:
/// off by one.  Widening the constant would only move the boundary
/// to the next block type, so the tail carrying a `/` is what
/// decides now, with `stat` and the seam candidate arbitrating.
#[test]
fn a_path_wrapped_short_of_the_pane_edge_still_joins() {
    let root = std::env::temp_dir()
        .join(format!("marspot-shortwrap-{}", std::process::id()));
    // A dotted directory gives the wrong answer somewhere real to
    // land — without it the test could pass for the wrong reason.
    let deep = root.join(".cache");
    std::fs::create_dir_all(&deep).unwrap();
    let file = deep.join("20260906-gpt6-headline-strategy-for-claude.md");
    std::fs::write(&file, b"x").unwrap();
    let p = file.display().to_string();

    // Split mid-word, and leave the first row well short of the
    // pane edge — which is what claudecode actually produces.
    let cut = p.find("gpt6-").map(|i| i + "gpt6-".len()).unwrap();
    let (a, b) = p.split_at(cut);
    let short_of_edge = (a.chars().count() as u16) + 8;

    let rows = [a, b];
    let mut src = StrSource::new(&rows, short_of_edge);
    src.cursor = (0, 1);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    let dir_only = format!("{}/", root.display());
    std::fs::remove_dir_all(&root).ok();

    assert!(
        texts.iter().any(|t| *t == p),
        "wrapped path not joined: {texts:?}",
    );
    assert!(
        !texts.iter().any(|t| *t == dir_only),
        "fell back to the directory: {texts:?}",
    );
}

/// 2026-08-18 field report: the URL ending one bullet came out
/// with the NEXT bullet's `-` glued to its tail.  The item above
/// ends flush at the pane edge on a path character, and a nested
/// item's indent is indistinguishable from a hanging wrap indent
/// — geometry says "continuation", the marker says otherwise.
#[test]
fn a_following_list_marker_is_not_a_wrap_continuation() {
    let head = "- 村子 → http://192.168.50.20:6031/index.html?page=pages/village/index";
    let cols = head.chars().count() as u16; // flush at the edge
    for next in [
        "  - 阿云的屋 → http://a.example/x", // the field report, verbatim
        "- room -> http://a.example/x",      // no indent
        "  - room -> http://a.example/x",    // hanging-indent shape
        "  * room -> http://a.example/x",
        "  2. room -> http://a.example/x",
    ] {
        let src = StrSource::new(&[head, next], cols);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        assert!(
            links.iter().any(|l| l.text
                == "http://192.168.50.20:6031/index.html?page=pages/village/index"),
            "next={next:?} must not extend the bullet above, got {links:?}"
        );
    }
}

/// The marker gate must not cost the merge its real job: a row
/// that resumes mid-token still joins, marker chars or not.
#[test]
fn a_mid_token_continuation_still_joins_after_the_marker_gate() {
    let head = "- 村子 → http://192.168.50.20:6031/index.html?page=pages/village/index";
    let cols = head.chars().count() as u16;
    let src = StrSource::new(&[head, "  -more/parts here"], cols);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    assert!(
        links.iter().any(|l| l.text
            == "http://192.168.50.20:6031/index.html?page=pages/village/index-more/parts"),
        "a token tail that merely starts with `-` still belongs to the row above, got {links:?}"
    );
}

/// Three complete paths in a row, each ending a few columns shy
/// of the right edge, must stay three links.
///
/// 2026-08-19 field report: a `ls`-style listing inside a TUI (two
/// spaces of indent, absolute paths) came out with the first and
/// second entries unlinked and the third linked.  The merge is
/// working as designed — a row ending within the path slack of the
/// edge, followed by an indented row, is exactly the shape of a
/// wrapped path — so the three rows become one logical line whose
/// text is `…/a.sql/…/bb//…/ccc/`.  Nothing is wrong with that;
/// the seam retry exists to take such a line apart again.
///
/// What was wrong: the retry sat behind `looks_like_path(whole
/// span)`, and the whole span is a concatenation of paths, which
/// does not look like a path.  The guard therefore rejected
/// precisely the input the retry was written for, and it had the
/// right answer at every step without ever being asked.
#[test]
fn consecutive_near_flush_paths_stay_separate_links() {
    let dir = std::env::temp_dir().join("marspot-linkify-seam-rows");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bb")).expect("mkdir bb");
    std::fs::create_dir_all(dir.join("ccc")).expect("mkdir ccc");
    std::fs::write(dir.join("a.sql"), b"x").expect("write a.sql");
    let d = format!("{}/", dir.to_string_lossy());

    let rows = [format!("  {d}a.sql"),
        format!("  {d}bb/"),
        format!("  {d}ccc/")];
    // Two columns wider than the longest row: every row now ends
    // inside the slack, which is what triggers the merge.
    let cols = (rows.iter().map(|r| r.chars().count()).max().unwrap() + 2) as u16;
    let refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
    let src = StrSource::new(&refs, cols);

    let mut texts: Vec<String> = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() })
        .into_iter()
        .map(|l| l.text)
        .collect();
    texts.sort();
    texts.dedup();
    assert_eq!(
        texts,
        vec![
            format!("{d}a.sql"),
            format!("{d}bb/"),
            format!("{d}ccc/"),
        ],
        "each row is a complete path and must keep its own link"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 2026-09-06 field report, inside codex: the path came out
/// underlined only as far as `…/lab36-continus/`, dropping the
/// `.cache/…` tail and the wrapped remainder.
///
/// The geometry is taken off the pane it happened in — 73 columns,
/// codex's `›` prompt glyph, and codex's own line break with a
/// two-space hanging indent (no DECAWM flag, because codex wrapped
/// it, not the terminal).  The scanner handles this shape; what
/// failed was `tui_mode` being off for that pane, which is fixed
/// on the marspot side.  Held here because the shape is the thing
/// that has to keep working.
#[test]
fn a_codex_wrapped_path_with_a_dot_directory_survives() {
    let root = std::env::temp_dir()
        .join(format!("marspot-linkify-codexwrap-{}", std::process::id()));
    let deep = root.join(".cache");
    std::fs::create_dir_all(&deep).unwrap();
    let file = deep.join("20260906-long-note-that-wrapped-far.md");
    std::fs::write(&file, b"x").unwrap();
    let full = file.display().to_string();

    // Break it where codex would: leaving the tail on the next row
    // behind a hanging indent.
    let cut = full.len() - "that-wrapped-far.md".len();
    let head = format!("\u{203a} {}", &full[..cut]);
    let tail = format!("  {}", &full[cut..]);
    // The real row sat two columns short of the pane edge (71 of
    // 73); keep that slack, since "ends flush" is a signal the
    // continuation test reads.
    let cols = head.chars().count() as u16 + 2;
    let mut src = StrSource::new(&[&head, &tail], cols);
    src.soft_wrapped[1] = false; // codex wrapped it, not the terminal

    let on = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let off = scan_visible_links(&src, ScanOpts { tui_mode: false, ..Default::default() });
    std::fs::remove_dir_all(&root).ok();

    assert!(
        on.iter().all(|l| l.text == full),
        "an agent TUI's own wrap must merge: {on:?}"
    );
    assert!(!on.is_empty(), "the path vanished entirely");
    assert!(
        off.iter().all(|l| l.text != full),
        "without tui_mode the merge is not attempted — this is what \
         made the pane's mode the whole ballgame: {off:?}"
    );
}
