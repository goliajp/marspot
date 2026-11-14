//! A size written into the interface goes through the display's scale.
//!
//! The sidebar was once sized in constants that were physical pixels,
//! on a display where a pixel and a window unit were the same thing,
//! and came out half its size on every other.  Nothing about the code
//! looked wrong.  This reads the source for the two shapes that
//! mistake has:
//!
//! 1. **A constant named for physical pixels** (`…_PX`, `…PHYS…`) in
//!    the layout, the theme's measurements, the frame builder or a
//!    component.  The ones that are meant — the hairlines — are listed
//!    below, and any other is a size nobody multiplied.
//! 2. **A length used bare.**  The theme's lengths are plain numbers in
//!    points or window units; one used in the frame builder or a
//!    component outside `px(…)` (or another call that scales what it
//!    is given), outside a multiplication by a scale, outside the
//!    definition of another constant and outside a function named
//!    `…_pt`, which adds lengths up in points, has become a number of
//!    pixels.
//!    In the layout, which keeps its own constants, a line that names
//!    one must multiply by the display's scale.

use std::collections::BTreeSet;
use std::path::Path;

/// The lines one physical pixel thick at every scale, by intent: the
/// table in `ui::theme::spec` says what each is.  Also a bar's least
/// visible width, which is a threshold in pixels and not a size.
const PHYSICAL: &[&str] = &["BORDER_PX", "SEPARATOR_PX", "DAY_RULE_PX", "BAR_MIN_PX"];

/// Component files that draw nothing.
const NOT_SCANNED: &[&str] = &["_tests.rs", "density_kit.rs", "panel_script.rs"];

/// What an `f64` constant is when it is not a length.
const NOT_A_LENGTH: &[&str] =
    &["_PX", "_FRAC", "_CELLS", "_AT", "_EM", "_RATIO", "_INK", "_GUTTERS", "_SECS", "_ALPHA", "LIGHTEN"];

/// Calls that scale what they are handed: the conversions themselves,
/// the tree's point, and two painters that take a length in points.
const SCALES_ITS_ARGUMENT: &[&str] = &["px", "snap", "snap_up", "snap_down", "Pt", "fill", "paint_cross"];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// A file's code: no comments, and nothing from its test module on.
fn code(src: &str) -> String {
    let body = src.split("\n#[cfg(test)]\nmod ").next().unwrap_or(src);
    body.lines().map(|l| l.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n")
}

fn is_const_name(s: &str) -> bool {
    s.len() > 1
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// `(name, type)` of every constant declared in `code`.
fn constants(code: &str) -> Vec<(String, String)> {
    code.lines()
        .filter_map(|l| {
            let decl = l.trim().trim_start_matches("pub ").trim_start_matches("pub(crate) ").strip_prefix("const ")?;
            let (name, rest) = decl.split_once(':')?;
            let ty = rest.split('=').next()?.trim();
            is_const_name(name.trim()).then(|| (name.trim().to_string(), ty.to_string()))
        })
        .collect()
}

fn named_for_pixels(name: &str) -> bool {
    name.ends_with("_PX") || name.contains("_PX_") || name.contains("PHYS")
}

/// The files of the frame builder and the components that are scanned.
fn painted() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in ["src/frame_build", "src/ui/components"] {
        for entry in std::fs::read_dir(root().join(dir)).expect("the directory").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".rs") && !NOT_SCANNED.iter().any(|x| name.ends_with(x)) {
                let src = std::fs::read_to_string(entry.path()).expect("a source file");
                out.push((format!("{dir}/{name}"), code(&src)));
            }
        }
    }
    out.sort();
    out
}

fn theme() -> Vec<(String, String)> {
    ["src/ui/theme/spec.rs", "src/ui/theme/spec/chrome.rs", "crates/marspot-term/src/layout.rs"]
        .map(|f| (f.to_string(), code(&std::fs::read_to_string(root().join(f)).expect("a source file"))))
        .to_vec()
}

/// Constants named for physical pixels in `files`, as `file: NAME`,
/// leaving out the ones that are meant.
fn pixel_names(files: &[(String, String)]) -> Vec<String> {
    files
        .iter()
        .flat_map(|(file, code)| constants(code).into_iter().map(move |(name, _)| (file, name)))
        .filter(|(_, name)| named_for_pixels(name) && !PHYSICAL.contains(&name.as_str()))
        .map(|(file, name)| format!("{file}: {name}"))
        .collect()
}

/// The names that are lengths: declared `f64` somewhere, never as
/// anything else, and not marked as a count, a fraction or a ratio.
fn lengths(files: &[(String, String)]) -> BTreeSet<String> {
    let all: Vec<(String, String)> = files.iter().flat_map(|(_, code)| constants(code)).collect();
    all.iter()
        .filter(|(name, ty)| ty == "f64" && !NOT_A_LENGTH.iter().any(|s| name.ends_with(s)))
        .filter(|(name, _)| all.iter().all(|(n, t)| n != name || t == "f64"))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Where `code` uses one of `lengths` through a path (`chrome::X`,
/// `spec::card::X`) with nothing scaling it: `(line, constant)`.
fn bare_lengths(code: &str, lengths: &BTreeSet<String>) -> Vec<(usize, String)> {
    let bytes = code.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = code[i..].find("::").map(|p| p + i) {
        i = at + 2;
        let end = (i..bytes.len()).find(|&j| !ident(bytes[j])).unwrap_or(bytes.len());
        let name = &code[i..end];
        if !is_const_name(name) || !lengths.contains(name) || code[end..].starts_with("::") {
            continue;
        }
        // the statement the use is in, and the calls it is inside
        let start = code[..at].rfind([';', '{', '}']).map_or(0, |p| p + 1);
        let statement = &code[start..code[end..].find([';', '{', '}']).map_or(code.len(), |p| p + end)];
        let defines_a_constant = statement.trim_start().trim_start_matches("pub ").starts_with("const ");
        let line = code[..end].rfind('\n').map_or(0, |p| p + 1);
        let line = &code[line..code[end..].find('\n').map_or(code.len(), |p| p + end)];
        let times_a_scale = line.contains("scale") || line.contains("density()");
        let mut depth = 0;
        let mut scaled = false;
        for j in (start..at).rev() {
            match bytes[j] {
                b')' => depth += 1,
                b'(' if depth > 0 => depth -= 1,
                b'(' => {
                    let from = (0..j).rev().find(|&k| !ident(bytes[k])).map_or(0, |k| k + 1);
                    scaled |= SCALES_ITS_ARGUMENT.contains(&&code[from..j]);
                }
                _ => {}
            }
        }
        // a function named `…_pt` adds lengths up in points, for its caller to scale
        let in_points = code[..at].rfind("fn ").is_some_and(|f| code[f + 3..].split('(').next().is_some_and(|n| n.ends_with("_pt")));
        if !scaled && !defines_a_constant && !times_a_scale && !in_points {
            out.push((code[..at].matches('\n').count() + 1, name.to_string()));
        }
    }
    out
}

/// Lines of the layout that name one of its own lengths and do not
/// multiply by the display's scale.
fn unscaled_layout_lines(code: &str) -> Vec<String> {
    let own = lengths(&[(String::new(), code.to_string())]);
    code.lines()
        .filter(|l| !l.trim_start().trim_start_matches("pub ").starts_with("const "))
        .filter(|l| l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| own.contains(w)))
        .filter(|l| !l.contains("density()") && !l.contains("scale"))
        .map(|l| l.trim().to_string())
        .collect()
}

#[test]
fn no_size_is_named_for_physical_pixels_but_the_hairlines() {
    let mut files = theme();
    files.extend(painted());
    let found = pixel_names(&files);
    assert!(
        found.is_empty(),
        "constants named for physical pixels: {found:?}. A size is in window units or points and is \
         multiplied by the display's scale where it is used; a line meant to stay one pixel thick goes \
         in the table in `ui::theme::spec` and in `PHYSICAL` here",
    );
    // the list holds nothing that is not there any more
    let declared: BTreeSet<String> = files.iter().flat_map(|(_, c)| constants(c)).map(|(n, _)| n).collect();
    for name in PHYSICAL {
        assert!(declared.contains(*name), "`{name}` is in `PHYSICAL` and no file declares it");
    }
}

#[test]
fn no_length_is_used_without_the_displays_scale() {
    let mut declared = theme();
    declared.extend(painted());
    let lengths = lengths(&declared);
    assert!(lengths.len() > 60, "only {} lengths were found; the theme was not read", lengths.len());
    let found: Vec<String> = painted()
        .iter()
        .flat_map(|(file, code)| {
            bare_lengths(code, &lengths).into_iter().map(move |(line, name)| format!("{file}:{line}: {name}"))
        })
        .collect();
    assert!(
        found.is_empty(),
        "lengths used as a number of pixels: {found:#?}. Wrap each in `spec::px(…)` or `chrome::px(…)`",
    );
    let layout = &theme()[2].1;
    let lines = unscaled_layout_lines(layout);
    assert!(lines.is_empty(), "the layout uses a length without `density()` or a scale: {lines:#?}");
}

/// Both checks find what they are for in code as it was written on the
/// day the sidebar came out half size, and pass its correction.
#[test]
fn the_checks_catch_the_shapes_the_mistake_has() {
    let was = "pub const SIDEBAR_ROW_H_PHYS: f64 = 22.0;\nconst ROW_INSET_PX: f64 = 4.0;\nconst BORDER_PX: f32 = 1.0;\n";
    let found = pixel_names(&[("layout.rs".to_string(), was.to_string())]);
    assert_eq!(found, ["layout.rs: SIDEBAR_ROW_H_PHYS", "layout.rs: ROW_INSET_PX"], "a hairline is let through");

    let lengths: BTreeSet<String> = ["SIDEBAR_ROW_H", "SIDEBAR_DOT", "ROW_PAD_Y"].map(String::from).into();
    let bare = "let dot = Rect {\n    x: rect.x + chrome::SIDEBAR_DOT,\n    h: chrome::px(chrome::SIDEBAR_DOT),\n};\n";
    assert_eq!(bare_lengths(bare, &lengths), [(2, "SIDEBAR_DOT".to_string())]);
    let fine = "let h = chrome::px(chrome::SIDEBAR_ROW_H);\nconst ROW_H: f64 = 2.0 * card::ROW_PAD_Y;\n\
                let y = chrome::SIDEBAR_DOT * state.scale;\nfill(p, rect, bg, c::SIDEBAR_DOT);\n";
    assert_eq!(bare_lengths(fine, &lengths), []);

    let layout = "const SIDEBAR_ROW_H: f64 = 22.0;\nlet a = SIDEBAR_ROW_H * density();\nlet b = i as f64 * SIDEBAR_ROW_H;\n";
    assert_eq!(unscaled_layout_lines(layout), ["let b = i as f64 * SIDEBAR_ROW_H;"]);
}
