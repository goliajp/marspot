//! The frame, header and content area every panel shares.
//!
//! A panel is, top to bottom: a frame, a header, a content area.  This
//! module is the only place that knows where those are, so the title
//! of one panel sits where the title of the next one does, and the
//! close button is under the pointer in the same place whichever panel
//! is open.
//!
//! [`PanelChrome`] is geometry first — built from the frame rect with
//! no painter, so a click handler asks it the same questions the
//! painter does — and [`PanelChrome::paint`] draws what the geometry
//! describes.  Below it are the pieces content is made of: a group
//! heading, a card, the line between two rows of a card.
//!
//! All rects are physical pixels.

use marspot_term::layout::Rect;

use crate::ui::components::controls;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::{Backdrop, View, ViewStyle};
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::{self, card as card_spec, content, header, panel, space};

/// Where a panel's shared parts are.
#[derive(Debug, Clone, Copy)]
pub struct PanelChrome {
    /// The whole panel.
    pub frame: Rect,
    /// The band across the top: title, meta line, close button.  Also
    /// the handle a draggable panel is dragged by.
    pub header: Rect,
    /// The close button's hit box, inside `header`.
    pub close: Rect,
    /// Where the panel's own content goes: under the header, inset
    /// from the frame's left, right and bottom edges.
    pub content: Rect,
    /// The window is dimmed from this y down; the strip above it (the
    /// window's own toolbar) stays as it is.
    pub backdrop_top: f64,
}

impl PanelChrome {
    /// The chrome of the panel at `frame`.  `top_inset` is the height
    /// of the window's own chrome above the panes.
    pub fn new(frame: Rect, top_inset: f64) -> Self {
        let header_h = spec::px(header::H).min(frame.h);
        let close_d = spec::px(header::CLOSE_SIZE);
        let pad_x = spec::px(content::PAD_X);
        let header = Rect { h: header_h, ..frame };
        Self {
            frame,
            header,
            close: Rect {
                x: frame.x + frame.w - spec::px(header::CLOSE_PAD_RIGHT) - close_d,
                y_top: frame.y_top + (header_h - close_d) * 0.5,
                w: close_d,
                h: close_d,
            },
            content: Rect {
                x: frame.x + pad_x,
                y_top: frame.y_top + header_h,
                w: (frame.w - 2.0 * pad_x).max(0.0),
                h: (frame.h - header_h - spec::px(content::PAD_BOTTOM)).max(0.0),
            },
            backdrop_top: top_inset,
        }
    }

    /// Left edge of the title — the content area's left edge.
    pub fn title_x(&self) -> f64 {
        self.content.x
    }

    /// Baseline that centres the title's capitals in the header.
    pub fn title_baseline(&self) -> f64 {
        self.header.y_top + (self.header.h + spec::px(header::TITLE.cap())) * 0.5
    }

    /// Right edge of the optional meta line, left of the close button.
    pub fn meta_right(&self) -> f64 {
        self.close.x - spec::px(header::META_GAP)
    }

    /// Is `(x, y)` on the close button?
    pub fn hit_close(&self, x: f64, y: f64) -> bool {
        self.close.contains(x, y)
    }

    /// Is `(x, y)` on the header where it is a drag handle — anywhere
    /// in it but the close button?
    pub fn hit_header(&self, x: f64, y: f64) -> bool {
        self.header.contains(x, y) && !self.hit_close(x, y)
    }

    /// Dims the window, draws the frame and the header, then hands
    /// `body` the painter and the content rect.
    ///
    /// Nothing is clipped: content that does not fit the content rect
    /// has to be left out by the panel itself.
    pub fn paint(
        &self,
        p: &mut ViewPainter,
        title: &str,
        meta: Option<&str>,
        close_hovered: bool,
        body: impl FnOnce(&mut ViewPainter, Rect),
    ) {
        self.frame_view().paint(p, |p| {
            p.panel_text(
                header::TITLE,
                self.title_x() as f32,
                self.title_baseline() as f32,
                title,
                palette::fg(),
            );
            if let Some(meta) = meta {
                let w = p.panel_text_width(header::META, meta) as f64;
                let baseline = p.ui_baseline_centred(
                    self.header.y_top as f32,
                    self.header.h as f32,
                    header::META.pt(),
                );
                p.panel_text(
                    header::META,
                    (self.meta_right() - w) as f32,
                    baseline,
                    meta,
                    palette::fg_muted(),
                );
            }
            paint_close(p, self.close, close_hovered);
            body(p, self.content);
        });
    }

    /// The dimmed window and the frame, with nothing in it.
    pub fn frame_view(&self) -> View {
        View {
            rect: self.frame,
            style: ViewStyle {
                bg: palette::frame_bg(),
                border_color: palette::frame_border(),
                border_width: panel::BORDER_PX,
                corner_radius: spec::px(panel::RADIUS) as f32,
                shadow_blur: spec::px(panel::SHADOW_BLUR) as f32,
                shadow_alpha: panel::SHADOW_ALPHA,
                padding: 0.0,
                backdrop: Backdrop::Dim {
                    color: palette::backdrop(),
                    exclude_above_y: self.backdrop_top,
                },
            },
        }
    }
}

/// A panel's close button, in its hit box `rect`.
pub fn paint_close(p: &mut ViewPainter, rect: Rect, hovered: bool) {
    let mark = if hovered {
        p.fill_rounded_rect(
            rect,
            palette::close_hover_bg(),
            spec::px(header::CLOSE_RADIUS) as f32,
            (palette::none(), 0.0),
        );
        palette::fg()
    } else {
        palette::fg_muted()
    };
    controls::paint_cross(p, rect, header::CLOSE_MARK, mark);
}

/// The frame for a panel that wants to be `w × h`: centred in the
/// window, no larger than the window allows, clear of the window's own
/// chrome at the top.
pub fn frame_rect(window_w: f64, window_h: f64, w: f64, h: f64, top_inset: f64) -> Rect {
    // Whole pixels, all four: a frame centred on a half pixel has its
    // border and everything measured from it antialiased across two
    // rows, and the same header then differs by a shade between two
    // panels that only differ in height.
    // Up, not to nearest: a frame a fraction shorter than its content
    // loses its last row to the cull.
    let w = spec::snap_up(w.min(window_w * panel::MAX_W_FRAC));
    let h = spec::snap_up(h.min(window_h * panel::MAX_H_FRAC));
    Rect {
        x: spec::snap((window_w - w) * 0.5),
        y_top: spec::snap(((window_h - h) * 0.5).max(top_inset + spec::px(panel::TOP_CLEARANCE))),
        w,
        h,
    }
}

/// Height of the frame whose content is `content_h` tall: the header
/// above it and the bottom padding below.
pub fn frame_height(content_h: f64) -> f64 {
    spec::px(header::H) + content_h + spec::px(content::PAD_BOTTOM)
}

/// From the top of a group heading's line to the top of the card
/// under it.
pub fn heading_height() -> f64 {
    spec::px(content::HEADING.cap() + space::HEADING_TO_CARD)
}

/// A group heading whose line starts at `top`: `title` at the left
/// edge `x`, `trailing` — a caption, if any — ending at `x + w`.
/// Returns the y where the group's card starts.
pub fn section_heading(
    p: &mut ViewPainter,
    x: f64,
    top: f64,
    w: f64,
    title: &str,
    trailing: Option<&str>,
) -> f64 {
    let baseline = (top + spec::px(content::HEADING.cap())) as f32;
    p.panel_text(content::HEADING, x as f32, baseline, title, palette::fg());
    if let Some(trailing) = trailing {
        let tw = p.panel_text_width(content::HEADING_TRAILING, trailing) as f64;
        p.panel_text(
            content::HEADING_TRAILING,
            (x + w - tw) as f32,
            baseline,
            trailing,
            palette::fg_muted(),
        );
    }
    top + heading_height()
}

/// A card: the surface a group's rows or table sit on.
pub fn card(p: &mut ViewPainter, rect: Rect) {
    p.fill_rounded_rect(
        rect,
        palette::card_bg(),
        spec::px(card_spec::RADIUS) as f32,
        (palette::card_border(), card_spec::BORDER_PX),
    );
}

/// The line between two rows of `card`, its top edge at `y`.
pub fn row_separator_rect(card: Rect, y: f64) -> Rect {
    let inset = spec::px(card_spec::SEPARATOR_INSET);
    Rect {
        x: card.x + inset,
        y_top: y,
        w: (card.w - 2.0 * inset).max(0.0),
        h: card_spec::SEPARATOR_PX,
    }
}

/// Draws the line [`row_separator_rect`] describes.
pub fn row_separator(p: &mut ViewPainter, card: Rect, y: f64) {
    // In the rounded-rect pass, like the card: the plain-rect pass is
    // drawn before it, so a line submitted there ends up under the
    // card it is meant to divide.
    p.fill_rounded_rect(row_separator_rect(card, y), palette::separator(), 0.0, (palette::none(), 0.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn panel_chrome_header_and_content_tile_the_frame() {
        let f = frame(100.0, 60.0, 880.0, 700.0);
        let c = PanelChrome::new(f, 0.0);
        assert_eq!((c.header.x, c.header.y_top, c.header.w), (f.x, f.y_top, f.w));
        assert_eq!(c.header.h, spec::px(header::H));
        assert_eq!(c.content.y_top, c.header.y_top + c.header.h, "content starts where the header ends");
        let pad = spec::px(content::PAD_X);
        assert_eq!(c.content.x - f.x, pad);
        assert_eq!(f.x + f.w - (c.content.x + c.content.w), pad, "same inset left and right");
        assert_eq!(
            f.y_top + f.h - (c.content.y_top + c.content.h),
            spec::px(content::PAD_BOTTOM),
        );
        assert_eq!(c.title_x(), c.content.x, "the title starts on the content's left edge");
    }

    #[test]
    fn panel_chrome_close_button_sits_in_the_header() {
        let f = frame(100.0, 60.0, 880.0, 700.0);
        let c = PanelChrome::new(f, 0.0);
        let d = spec::px(header::CLOSE_SIZE);
        assert_eq!((c.close.w, c.close.h), (d, d));
        assert_eq!(f.x + f.w - (c.close.x + c.close.w), spec::px(header::CLOSE_PAD_RIGHT));
        let above = c.close.y_top - c.header.y_top;
        let below = c.header.y_top + c.header.h - (c.close.y_top + c.close.h);
        assert_eq!(above, below, "centred in the header");
        assert!(above > 0.0);
        assert_eq!(c.close.x - c.meta_right(), spec::px(header::META_GAP));
        // The title's capitals are centred on the same line.
        let cap = spec::px(header::TITLE.cap());
        let title_mid = c.title_baseline() - cap * 0.5;
        assert!((title_mid - (c.header.y_top + c.header.h * 0.5)).abs() < 1e-9);
    }

    #[test]
    fn panel_chrome_hits_close_and_header_apart() {
        let c = PanelChrome::new(frame(100.0, 60.0, 880.0, 700.0), 0.0);
        let (cx, cy) = (c.close.x + c.close.w * 0.5, c.close.y_top + c.close.h * 0.5);
        assert!(c.hit_close(cx, cy));
        assert!(!c.hit_header(cx, cy), "the close button is not a drag handle");
        assert!(c.hit_header(c.title_x(), cy));
        assert!(!c.hit_close(c.title_x(), cy));
        let in_content = c.content.y_top + 10.0;
        assert!(!c.hit_header(c.title_x(), in_content));
        assert!(!c.hit_close(cx, in_content));
        assert!(!c.hit_header(c.frame.x - 1.0, cy), "outside the frame is outside the header");
    }

    /// The point of sharing the chrome: whatever a panel's size, its
    /// header is laid out the same from the frame's corners.
    #[test]
    fn panel_chrome_header_is_the_same_in_every_frame() {
        let a = PanelChrome::new(frame(0.0, 0.0, 600.0, 400.0), 0.0);
        let b = PanelChrome::new(frame(321.0, 77.0, 1900.0, 1000.0), 40.0);
        assert_eq!(a.header.h, b.header.h);
        assert_eq!(a.title_x() - a.frame.x, b.title_x() - b.frame.x);
        assert_eq!(a.title_baseline() - a.frame.y_top, b.title_baseline() - b.frame.y_top);
        let right = |c: &PanelChrome| c.frame.x + c.frame.w - c.close.x;
        assert_eq!(right(&a), right(&b));
        assert_eq!(a.close.y_top - a.frame.y_top, b.close.y_top - b.frame.y_top);
        assert_eq!(b.backdrop_top, 40.0);
    }

    #[test]
    fn panel_chrome_frame_is_centred_and_fits_the_window() {
        let r = frame_rect(2000.0, 1200.0, 880.0, 600.0, 0.0);
        assert_eq!((r.w, r.h), (880.0, 600.0));
        assert_eq!((r.x, r.y_top), (560.0, 300.0));
        // Wanting more than the window has is cut to its share of it.
        let r = frame_rect(1000.0, 800.0, 5000.0, 5000.0, 0.0);
        assert_eq!(r.w, 1000.0 * panel::MAX_W_FRAC);
        assert_eq!(r.h, 800.0 * panel::MAX_H_FRAC);
        assert!(r.x >= 0.0 && r.x + r.w <= 1000.0);
        assert!(r.y_top >= 0.0 && r.y_top + r.h <= 800.0);
        // The window's own chrome is never covered.
        let r = frame_rect(1000.0, 800.0, 600.0, 700.0, 120.0);
        assert_eq!(r.y_top, 120.0 + spec::px(panel::TOP_CLEARANCE));
    }

    #[test]
    fn panel_chrome_frame_height_wraps_its_content() {
        let content_h = 333.0;
        let f = frame(0.0, 0.0, 800.0, frame_height(content_h));
        assert!((PanelChrome::new(f, 0.0).content.h - content_h).abs() < 1e-9);
    }

    #[test]
    fn panel_chrome_separator_is_inset_from_both_card_edges() {
        let card = frame(40.0, 100.0, 600.0, 200.0);
        let s = row_separator_rect(card, 150.0);
        let inset = spec::px(card_spec::SEPARATOR_INSET);
        assert_eq!(s.x - card.x, inset);
        assert_eq!(card.x + card.w - (s.x + s.w), inset);
        assert_eq!((s.y_top, s.h), (150.0, card_spec::SEPARATOR_PX));
    }

    /// `src` without its comments and without its tests module.  A
    /// `#[cfg(test)]` on anything but a module is not the end of the
    /// file's code.
    fn code_of(src: &str) -> String {
        let end = ["#[cfg(test)]\nmod ", "#[cfg(test)]\npub(crate) mod ", "#[cfg(test)]\npub mod "]
            .iter()
            .filter_map(|m| src.find(m))
            .min()
            .unwrap_or(src.len());
        src[..end].lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Where `src` reaches into the theme table itself instead of
    /// asking `panel_palette` for a role.
    fn table_reads(src: &str) -> Vec<String> {
        let code = code_of(src);
        ["theme::color", "theme::table", "theme::current", "table::color"]
            .iter()
            .filter(|path| code.contains(*path))
            .map(|path| path.to_string())
            .collect()
    }

    /// Every `.rs` file under `dir`, with its path from `dir`.
    fn sources(dir: &std::path::Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("the source directory") {
                let path = entry.expect("a directory entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path.strip_prefix(dir).expect("under the directory").display().to_string();
                    out.push((name, std::fs::read_to_string(&path).expect("a source file")));
                }
            }
        }
        out
    }

    /// A number written with a decimal point, as a colour channel is.
    fn float(tok: &str) -> Option<f64> {
        let tok = tok.trim().trim_end_matches("f32").trim_end_matches("f64").trim_end_matches('_');
        (tok.contains('.') && tok.chars().all(|c| c.is_ascii_digit() || c == '.'))
            .then(|| tok.parse().ok())
            .flatten()
    }

    /// The colours `src` writes out by hand, outside its tests and its
    /// comments: a `Color` built from numbers, and a bracketed or
    /// parenthesised group of three or four values that starts with
    /// three numbers and has none outside 0..=1 — an RGB triple or an
    /// RGBA array, whichever way it is spelled.
    fn colour_literals(src: &str) -> Vec<String> {
        let code = code_of(src);
        let mut found = Vec::new();
        for ctor in ["Color::rgba(", "Color::rgb(", "Color::hex("] {
            // `CGColor::rgb(` is another type's constructor.
            let own = |i: usize| !code[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            found.extend(
                code.match_indices(ctor).filter(|(i, _)| own(*i)).map(|(i, _)| code[i..].chars().take(32).collect()),
            );
        }
        // A clear colour spelled as a struct: `ClearColor { red: 0.1, .. }`.
        for (i, _) in code.match_indices("ClearColor {") {
            let body = code[i..].split('}').next().unwrap_or_default();
            let red = body.split("red:").nth(1).and_then(|rest| rest.split(',').next());
            if red.is_some_and(|v| float(v).is_some()) {
                found.push(body.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        for (open, _) in code.match_indices(['[', '(']) {
            let close = if code.as_bytes()[open] == b'[' { ']' } else { ')' };
            let rest = &code[open + 1..];
            let Some(end) = rest.find(['[', ']', '(', ')']) else { continue };
            if !rest[end..].starts_with(close) {
                continue;
            }
            let body = &rest[..end];
            let unit = |v: f64| (0.0..=1.0).contains(&v);
            let is_colour = if let Some((value, count)) = body.split_once(';') {
                close == ']' && float(value).is_some_and(unit) && matches!(count.trim(), "3" | "4")
            } else {
                let parts: Vec<&str> = body.split(',').filter(|part| !part.trim().is_empty()).collect();
                matches!(parts.len(), 3 | 4)
                    && parts[..3].iter().all(|part| float(part).is_some())
                    && parts.iter().filter_map(|part| float(part)).all(unit)
            };
            if is_colour {
                found.push(body.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        found
    }

    /// The search has to be able to find one, in each spelling.
    #[test]
    fn panel_chrome_colour_search_finds_every_spelling() {
        // Assembled, so that this file does not contain one itself.
        let (o, c) = ('[', ']');
        for literal in [
            format!("bg: {o}0.13, 0.14, 0.17, 1.0{c},"),
            format!("fill: rgba8_of_f32({o}0.0, 0.0, 0.0, alpha{c}),"),
            format!("color: {o}0.0; 4{c}"),
            format!("const SEAM: (f32, f32, f32) = {}0.115, 0.130, 0.155{};", '(', ')'),
            format!("let bg = Color::{}(33, 36, 43, 1.0);", "rgba"),
            format!("color.setClearColor(MTL{} {{ red: 0.078, green: 0.086, blue: 0.110, alpha: 1.0 }});", "ClearColor"),
        ] {
            assert_eq!(colour_literals(&literal).len(), 1, "missed: {literal}");
        }
        // And what is not a colour is left alone: a pair, a list of
        // choices that leaves 0..=1, a comment, a test.
        for clean in [
            format!("shadow_offset: {o}0.0, 0.0{c},"),
            format!("const CHOICES: &{o}f32{c} = &{o}0.5, 1.0, 1.5, 2.5{c};"),
            format!("// was {o}0.13, 0.14, 0.17, 1.0{c}"),
            format!("MTL{} {{ red: c{o}0{c} as f64, green, blue, alpha: 1.0 }}", "ClearColor"),
            format!("fn f() {{}}\n#{o}cfg(test){c}\nmod tests {{ const C: {o}f32; 4{c} = {o}1.0; 4{c}; }}"),
        ] {
            assert!(colour_literals(&clean).is_empty(), "false alarm: {clean}");
        }
    }

    /// Every file that lays a frame out, paints a component or a view,
    /// or draws for the renderer takes its colours from `panel_palette`,
    /// the one place an entry of the theme table turns into a role.
    #[test]
    fn panel_chrome_source_has_no_colour_literals() {
        // The palette holds the one colour that is no theme's: none.
        // `Color` defines the constructors.
        const EXEMPT: [&str; 2] = ["panel_palette.rs", "color.rs"];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut scanned = 0;
        for dir in ["frame_build", "ui/components", "ui/core", "bin/marspot-shell"] {
            for (name, src) in sources(&root.join(dir)) {
                if EXEMPT.contains(&name.as_str()) {
                    continue;
                }
                let found = colour_literals(&src);
                assert!(found.is_empty(), "{dir}/{name} writes colours out by hand: {found:?}");
                scanned += 1;
            }
        }
        let renderer = std::fs::read_to_string(root.join("render_metal.rs")).expect("the renderer");
        // One, by name: what an offscreen render for a test starts from.
        let named = "const OFFSCREEN_CLEAR: MTLClearColor = MTLClearColor {";
        let found = colour_literals(&renderer);
        assert_eq!(found.len(), 1, "render_metal.rs writes colours out by hand: {found:?}");
        assert!(renderer.contains(named), "the renderer's one literal is not the offscreen clear");
        // A wrong path scans nothing and passes; the directories
        // together hold far more files than this.
        assert!(scanned >= 140, "only {scanned} files were scanned");
        // The exemptions are still what they claim to be.
        let palette = std::fs::read_to_string(root.join("ui/components/panel_palette.rs")).expect("the palette");
        assert!(!colour_literals(&palette).is_empty(), "the palette no longer needs its exemption");
    }

    /// The theme table is read in two places: the theme's own module
    /// and the role functions.  Everything else asks for a role, so a
    /// colour that does not follow the theme has nowhere to come from.
    #[test]
    fn panel_chrome_only_the_palette_reads_the_theme_table() {
        // Assembled, so that this file does not contain one itself.
        let direct = format!("let c = crate::ui::{}::color::fg();", "theme");
        assert_eq!(table_reads(&direct), ["theme::color"], "the search has to be able to find one");
        assert!(table_reads("// theme::table in a comment\nlet c = palette::fg();").is_empty());

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut scanned = 0;
        for (name, src) in sources(&root) {
            let allowed = name.starts_with("ui/theme/") || name == "ui/components/panel_palette.rs";
            if allowed {
                continue;
            }
            let found = table_reads(&src);
            assert!(found.is_empty(), "{name} reads the theme table directly: {found:?}");
            scanned += 1;
        }
        assert!(scanned >= 150, "only {scanned} files were scanned");
        let palette = std::fs::read_to_string(root.join("ui/components/panel_palette.rs")).expect("the palette");
        assert!(!table_reads(&palette).is_empty(), "the palette is where the table is read");
    }
}
