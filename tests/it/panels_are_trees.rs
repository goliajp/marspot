//! A panel's content is nodes of a tree, not calls on the painter.
//!
//! The five panels and the context menu are described as trees; what
//! reaches the painter directly is the shared controls a node is
//! painted by, the one chart, and the window's own chrome.  This reads
//! `src/ui/components` and holds every other file to drawing nothing
//! itself, so a panel cannot grow a second, hand-placed picture beside
//! its tree.

use std::path::Path;

/// The painter's drawing calls.  `text` is also what a tree's builder
/// calls a text node, so it counts only on a receiver named as the
/// painter is everywhere: `p` or `painter`.
const DRAWS: &[&str] = &[
    ".fill_rect(",
    ".fill_rounded_rect(",
    ".ui_text_at(",
    ".panel_text(",
    ".panel_text_in(",
    ".text_in(",
    ".ui_rects.push(",
    ".cells.push(",
    ".glyphs.push(",
    "p.text(",
    "painter.text(",
];

/// The controls and frames a tree's nodes are painted by.
const PRESETS: &[&str] = &["controls.rs", "panel_chrome.rs", "context_menu_view.rs"];

/// Content that is a drawing: the usage panel's timeline.
const DRAWINGS: &[&str] = &["cc_usage_paint.rs"];

/// The window's chrome, which is not a panel.
const CHROME: &[&str] = &[
    "button.rs",
    "sidebar.rs",
    "search_overlay.rs",
    "grid_seams.rs",
    "grid_item.rs",
    "list_view.rs",
    "text_input.rs",
];

/// Files that are tests or their kits.
fn is_test(name: &str) -> bool {
    name.ends_with("_tests.rs") || name == "density_kit.rs" || name == "panel_script.rs"
}

/// A file's code: no comments, and nothing from its test module on.
fn code(src: &str) -> String {
    let body = src.split("\n#[cfg(test)]\nmod ").next().unwrap_or(src);
    body.lines().map(|l| l.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n")
}

/// The lines of `src` that draw through the painter, numbered from 1.
fn drawing_lines(src: &str) -> Vec<usize> {
    code(src)
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            DRAWS.iter().any(|d| {
                l.match_indices(d).any(|(at, _)| {
                    // `p.text(` must not be the tail of `ui.text(` or `sp.text(`
                    !d.starts_with('p') || at == 0 || !l.as_bytes()[at - 1].is_ascii_alphanumeric() && l.as_bytes()[at - 1] != b'_'
                })
            })
        })
        .map(|(n, _)| n + 1)
        .collect()
}

fn may_draw(name: &str) -> bool {
    PRESETS.contains(&name) || DRAWINGS.contains(&name) || CHROME.contains(&name)
}

#[test]
fn no_panel_draws_its_content_through_the_painter() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/components");
    let mut found = Vec::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir).expect("the components directory") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if !name.ends_with(".rs") || is_test(&name) || may_draw(&name) {
            continue;
        }
        scanned += 1;
        let src = std::fs::read_to_string(&path).expect("a source file");
        for line in drawing_lines(&src) {
            found.push(format!("{name}:{line}"));
        }
    }
    assert!(scanned > 20, "only {scanned} files were read; the directory moved");
    assert!(found.is_empty(), "drawn through the painter outside a preset, a drawing or the chrome: {found:?}");
    // every file excused is there to be excused
    for name in PRESETS.iter().chain(DRAWINGS).chain(CHROME) {
        let src = std::fs::read_to_string(dir.join(name)).unwrap_or_else(|_| panic!("{name} is excused and is not there"));
        assert!(!drawing_lines(&src).is_empty(), "{name} is excused and draws nothing");
    }
}

/// The reader has to be able to find something: a panel's view that
/// places a rectangle or a run of text by hand is caught, and one that
/// only describes nodes is not.
#[test]
fn a_view_that_draws_by_hand_is_caught() {
    let by_hand = "fn frame(p: &mut ViewPainter<'_>) {\n    p.fill_rounded_rect(r, c, 4.0, b);\n    p.text(x, y, \"n\", c);\n    // p.fill_rect(old)\n    p.ui_rects.push(i);\n}\n";
    assert_eq!(drawing_lines(by_hand), [2, 3, 5]);
    let described = "fn describe(ui: &mut Ui<'_>) {\n    ui.text(font, ink, \"n\");\n    sp.text(1);\n    ui.leaf(TAG, w, h).fill(ink);\n}\n\n#[cfg(test)]\nmod tests {\n    fn t(p: &mut P) { p.fill_rect(r, c); }\n}\n";
    assert_eq!(drawing_lines(described), [] as [usize; 0]);
    assert!(!may_draw("settings_view.rs") && !may_draw("process_view.rs") && may_draw("controls.rs"));
}
