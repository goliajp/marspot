//! Where a wrapped path or file URL ends once its rows are joined.

use super::*;

/// 2026-09-04, the reported defect itself, reproduced from
/// session 385's bytelog: `…/notes/provenance-probe.sh -n 5`
/// underlined only as far as `…/spg/`.
///
/// claudecode broke that line at the space.  The break costs the
/// space twice over — the previous row's trailing blanks are
/// popped and this row's hanging indent is skipped — so the merge
/// produces `…probe.sh-n 5`, which is not a file.  Backing off
/// through punctuation then lands on the directory.
///
/// A break inside a word (`…exec-tax` + `-2026-09-04.md`) needs
/// exactly the seamless join this one must not have, and the two
/// are indistinguishable on the grid: both rows fill the width,
/// both continuations open with `-`.  So the seam is offered as a
/// candidate end rather than decided either way.
#[test]
fn a_cc_seam_where_the_line_broke_at_a_space_is_a_candidate_end() {
    // The shape needs a shorter prefix that DOES exist, or the
    // wrong answer has nowhere to land and the test passes for
    // the wrong reason.  A dotted directory supplies exactly that
    // — which is why the report ended at `…/spg/`.
    let root = std::env::temp_dir().join(format!("marspot-seam-{}", std::process::id()));
    let deep = root.join(".notes").join("docs");
    std::fs::create_dir_all(&deep).unwrap();
    let file = deep.join("probe.sh");
    std::fs::write(&file, b"x").unwrap();
    let p = file.display().to_string();

    let first = format!("  {p}");
    let rows = [first.as_str(), "  -n 5"];
    let cols = first.chars().count() as u16;
    let mut src = StrSource::new(&rows, cols);
    src.cursor = (6, 1);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    let root_s = format!("{}/", root.display());
    std::fs::remove_dir_all(&root).ok();

    assert!(
        texts.iter().any(|t| *t == p),
        "seam not offered as a path end, got {texts:?}",
    );
    assert!(
        !texts.iter().any(|t| *t == root_s),
        "fell back past the seam to a bare directory: {texts:?}",
    );
}

/// Reported 2026-09-16.  claudecode broke an absolute path right
/// before a `/`, and the file name on the next row had Chinese
/// glued to it with no space:
///
///   `交接文档写好了：/Users/…/buwanren/notes/rfcs`
///   `  /20260916-handoff.md。按项目规矩写在 …`
///
/// The link stopped at `…/rfcs` — a directory the line never
/// named.  The seam and the `。` are both candidate ends and both
/// exist on disk; the file is the longer one and has to win.
#[test]
fn a_seam_does_not_outrank_a_longer_end_that_exists() {
    let root = std::env::temp_dir().join(format!("marspot-seam-cjk-{}", std::process::id()));
    let dir = root.join("notes").join("rfcs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handoff.md"), b"x").unwrap();
    let d = dir.display().to_string();

    let first = format!("  {d}");
    let second = "  /handoff.md。按项目规矩写在";
    let rows = [first.as_str(), second];
    let cols = first.chars().count() as u16;
    let mut src = StrSource::new(&rows, cols);
    src.cursor = (0, 1);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
    std::fs::remove_dir_all(&root).ok();

    let want = format!("{d}/handoff.md");
    assert!(texts.iter().any(|t| *t == want), "got {texts:?}");
    assert!(!texts.iter().any(|t| *t == d), "stopped at the seam: {texts:?}");
}

/// The counter-case that keeps the seam a candidate rather than a
/// rule: a name really broken mid-word still matches whole,
/// because the full span is offered before any seam.
#[test]
fn a_word_broken_across_a_cc_seam_still_matches_whole() {
    let rows = ["  /etc/hos", "  ts"];
    let mut src = StrSource::new(&rows, 10);
    src.cursor = (4, 1);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    assert!(
        links.iter().any(|l| l.text == "/etc/hosts"),
        "seamless join broken: {links:?}",
    );
}

/// 2026-09-27 report: a `file://` URL wrapped across two rows of a
/// codex pane linked only as far as the row above's last
/// directory, and `vis/brand-dist/index-ja.html#principles` below
/// it stayed plain.
///
/// The rows merged fine.  What failed is the arbitration: a
/// `file://` link is checked against the disk, `#principles` is
/// not on the disk, the whole span therefore did not exist, and the
/// walk back to a shorter candidate stopped at the first one that
/// does — the directory ending the row above, right at the seam.
#[test]
fn a_wrapped_file_url_keeps_its_fragment_and_crosses_the_seam() {
    let root = std::env::temp_dir().join(format!("marspot-fragwrap-{}", std::process::id()));
    let dir = root.join("vis/brand-dist");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("index-ja.html");
    std::fs::write(&file, b"x").unwrap();
    let url = format!("file://{}#principles", file.display());

    // Wrap it the way codex does: break after a `/` near the right
    // edge, continuation hanging two columns in.
    let cols: u16 = 73;
    let indent = "  ";
    let chars: Vec<char> = url.chars().collect();
    let cut = (1..chars.len())
        .filter(|&k| chars[k - 1] == '/').rfind(|&k| {
            let head = indent.chars().count() + k;
            let tail = indent.chars().count() + (chars.len() - k);
            // Flush enough to read as a wrap, and the rest must fit.
            head <= cols as usize && head >= 50 && tail <= cols as usize
        });
    let Some(cut) = cut else {
        std::fs::remove_dir_all(&root).ok();
        panic!("no wrap point in {url} — the case this test is about cannot be built");
    };
    let row = |text: String| -> Vec<char> {
        let mut v: Vec<char> = text.chars().collect();
        v.truncate(cols as usize);
        while v.len() < cols as usize {
            v.push('\0');
        }
        v
    };
    let head: String = chars[..cut].iter().collect();
    let tail: String = chars[cut..].iter().collect();
    let src = StrSource {
        rows: vec![row(format!("{indent}{head}")), row(format!("{indent}{tail}"))],
        cols,
        cursor: (0, 0),
        soft_wrapped: vec![false; 2],
    };
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(links.len(), 2, "the link covers both rows: {links:?}");
    for l in &links {
        assert_eq!(l.text, url, "each row carries the whole URL");
        assert_eq!(l.kind, LinkKind::File, "it names a file, not a web page");
    }
}

/// Half a name is worse than none.
///
/// Same wrap as above, but the file it names is not there.  The
/// arbitration then walks back and the first candidate that does
/// exist is the directory ending the row above — a link over half
/// of what is drawn, pointing somewhere the line never named.
/// 「现在的问题是识别一半，你要么完全不识别也好」(2026-09-27), so
/// nothing is emitted.
#[test]
fn a_wrapped_name_that_does_not_exist_is_not_linked_by_half() {
    let root = std::env::temp_dir().join(format!("marspot-halfname-{}", std::process::id()));
    let dir = root.join("vis/brand-dist");
    std::fs::create_dir_all(&dir).unwrap();
    // Note: no file written — the name on screen is not there.
    let missing = dir.join("index-ja.html");
    let url = format!("file://{}", missing.display());

    let cols: u16 = 73;
    let indent = "  ";
    let chars: Vec<char> = url.chars().collect();
    let cut = (1..chars.len())
        .filter(|&k| chars[k - 1] == '/' && chars[k] != '/').rfind(|&k| {
            let head = indent.chars().count() + k;
            let tail = indent.chars().count() + (chars.len() - k);
            head <= cols as usize && head >= 50 && tail <= cols as usize
        });
    let Some(cut) = cut else {
        std::fs::remove_dir_all(&root).ok();
        panic!("no wrap point in {url} — the case this test is about cannot be built");
    };
    let row = |text: String| -> Vec<char> {
        let mut v: Vec<char> = text.chars().collect();
        v.truncate(cols as usize);
        while v.len() < cols as usize {
            v.push('\0');
        }
        v
    };
    let head: String = chars[..cut].iter().collect();
    let tail: String = chars[cut..].iter().collect();
    let src = StrSource {
        rows: vec![row(format!("{indent}{head}")), row(format!("{indent}{tail}"))],
        cols,
        cursor: (0, 0),
        soft_wrapped: vec![false; 2],
    };
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    std::fs::remove_dir_all(&root).ok();

    assert!(
        links.is_empty(),
        "a name broken across rows is linked whole or not at all: {links:?}",
    );
}

/// The field report's actual shape: the path hard-wrapped at the
/// pane edge *and* the sentence resumed after the `;`.  Both
/// repairs have to hold at once — the merge to reach the second
/// row, the cut to drop what follows.
#[test]
fn a_wrapped_path_with_prose_glued_to_its_end_is_one_link() {
    let p = std::env::temp_dir().join(format!(
        "marspot-linkify-wrapped-reply-b-section-2-{}.md",
        std::process::id()
    ));
    std::fs::write(&p, b"x").unwrap();
    let text = p.display().to_string();
    // Split the path so the first row ends flush at the right
    // edge, which is what claudecode's fixed-width wrap does.
    let split = text.len() - 12;
    let head = format!("  - note:{}", &text[..split]);
    let cols = head.chars().count() as u16;
    let tail = format!("  {};memory noted", &text[split..]);
    let src = StrSource::new(&[&head, &tail], cols);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let files: Vec<&LinkRange> = links.iter().filter(|l| l.kind == LinkKind::File).collect();
    assert!(!files.is_empty(), "wrapped path found no link at all");
    assert!(
        files.iter().all(|l| l.text == text),
        "every segment carries the whole path: {:?}",
        files.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    // One segment per physical row it crosses.
    assert_eq!(files.len(), 2, "both rows underline");
    let _ = std::fs::remove_file(&p);
}

/// The field report's full shape: the same gloss, but the path
/// also hard-wrapped mid-token at the pane edge, so the row merge
/// and the dash cut both have to land for the link to come out
/// whole.  A dot-directory sits in the middle — the break fell
/// inside it (`/.n` + `otes/`), which is what made the truncated
/// link look like a plausible one.
#[test]
fn a_wrapped_path_with_an_em_dash_gloss_is_one_link() {
    let dir = std::env::temp_dir()
        .join(format!("marspot-linkify-emdash-wrap-{}", std::process::id()))
        .join(".notes");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("paper1-discovers.html");
    std::fs::write(&p, b"x").unwrap();
    let text = p.display().to_string();
    // Cut the path so the first row ends flush at the right edge,
    // which is where claudecode's fixed-width wrap puts it.
    let split = text.chars().count() - 27;
    let head: String = format!("open {}", text.chars().take(split).collect::<String>());
    let cols = head.chars().count() as u16;
    let tail = format!(
        "  {}——页首是三条路与定论",
        text.chars().skip(split).collect::<String>()
    );
    let src = StrSource::new(&[&head, &tail], cols);
    let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
    let files: Vec<&LinkRange> = links.iter().filter(|l| l.kind == LinkKind::File).collect();
    assert!(!files.is_empty(), "wrapped path found no link at all");
    assert!(
        files.iter().all(|l| l.text == text),
        "every segment carries the whole path: {:?}",
        files.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    assert_eq!(files.len(), 2, "both rows underline");
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}
