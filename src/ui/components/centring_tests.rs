//! A label sits in the middle of its control: measured from the
//! picture, in English, Japanese and Chinese, at display scale 1 and 2.
//!
//! Every label of a sheet of segments, buttons and chips is found in
//! the rendered frame — the rows its control fills, and the rows its
//! ink is on — and the middle of the ink has to be within half a pixel
//! of the middle of the control.
//!
//! **What counts as ink.**  A row is ink where a pixel of it is at
//! least half way from the control's colour to the label's.  For a
//! label that starts with a capital letter the ink is that letter's: a
//! word with a `p` or a `g` in it hangs below the line, and centring it
//! on everything it draws would set `Apply` higher than `Cancel` in the
//! button beside it.  A label in Han or kana has no line to hang below
//! and is measured whole.
//!
//! The controls are placed on half pixels, where a row of a panel may
//! put them; they are drawn on whole ones.

use super::density_kit::capture_pixels;
use super::panel_snapshot_tests as panel;
use crate::snapshot_kit::in_language;
use crate::ui::components::controls::{self, ButtonKind};
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::strings::{t, Msg};
use crate::ui::theme::spec;
use marspot_term::language::LangId;
use marspot_term::layout::Rect;
use std::sync::Mutex;

/// English, 日本語 and 简体中文 in every language; Faster, さらに速い or
/// 更快; Apply, 適用 or 应用.
const LABELS: [Msg; 5] =
    [Msg::LangEnglish, Msg::LangJapanese, Msg::LangChinese, Msg::ScrollFaster, Msg::LayoutApply];

/// Where the sheet drew each control: `(label, kind, box, corner)`, the
/// last its corner radius in pixels.
static DRAWN: Mutex<Vec<(String, &'static str, Rect, f64)>> = Mutex::new(Vec::new());

fn sheet(p: &mut ViewPainter<'_>) {
    let whole = Rect { x: 0.0, y_top: 0.0, w: p.window_w, h: p.window_h };
    p.fill_rounded_rect(whole, palette::surface_base(), 0.0, (palette::none(), 0.0));
    let mut drawn = Vec::new();
    let (left, gap) = (spec::px(20.0) + 0.5, spec::px(12.0));

    let top = spec::px(20.0) + 0.5;
    let (w, h) = controls::segmented_size(&LABELS, &mut controls::font_measure(&mut *p.font));
    let control = Rect { x: left, y_top: top, w, h };
    controls::paint_segmented(p, control, &LABELS, Some(1), None, false);
    for (n, label) in LABELS.iter().enumerate() {
        let seg = controls::segment_rect(control, n, &LABELS, &mut controls::font_measure(&mut *p.font));
        drawn.push((t(*label).to_string(), "segment", spec::snap_rect(seg), spec::px(spec::control::SEGMENT_RADIUS)));
    }

    let top = top + h + gap;
    let mut x = left;
    for label in LABELS.map(t) {
        let (w, h) = controls::button_size(label, &mut controls::font_measure(&mut *p.font));
        let rect = Rect { x, y_top: top, w, h };
        controls::paint_button(p, rect, label, ButtonKind::Secondary, false, false);
        drawn.push((label.to_string(), "button", spec::snap_rect(rect), spec::px(spec::control::BUTTON_RADIUS)));
        x += w + gap;
    }

    let top = top + spec::px(spec::control::BUTTON_H) + gap;
    let mut x = left;
    for label in LABELS.map(t) {
        let (w, h) = controls::chip_size(label, &mut controls::font_measure(&mut *p.font));
        let rect = Rect { x, y_top: top, w, h };
        controls::paint_chip(p, rect, label, 0);
        drawn.push((label.to_string(), "chip", spec::snap_rect(rect), spec::px(spec::control::CHIP_RADIUS)));
        x += w + gap;
    }
    *DRAWN.lock().unwrap() = drawn;
}

fn scene() {
    panel::snapshot("centring", 1400, 260, |_, _| {
        *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap() = Some(sheet);
    });
}

struct Picture {
    rgba: Vec<u8>,
    w: usize,
}

impl Picture {
    fn at(&self, x: usize, y: usize) -> [i32; 3] {
        let i = (y * self.w + x) * 4;
        [self.rgba[i] as i32, self.rgba[i + 1] as i32, self.rgba[i + 2] as i32]
    }
}

fn far(a: [i32; 3], b: [i32; 3]) -> i32 {
    (0..3).map(|i| (a[i] - b[i]).abs()).max().unwrap_or(0)
}

/// What the picture shows of one control: the rows it fills, and the
/// rows its label's ink is on, both first to last.
struct Measured {
    rows: (usize, usize),
    ink: (usize, usize),
}

impl Measured {
    /// How far below the control's middle the ink's middle is, in pixels.
    fn off_centre(&self) -> f64 {
        (self.ink.0 + self.ink.1) as f64 * 0.5 - (self.rows.0 + self.rows.1) as f64 * 0.5
    }
}

fn measure(pic: &Picture, label: &str, rect: Rect, corner: f64) -> Measured {
    // clear of the rounded corners, and of the label: a control pads
    // its label by more than its corner radius
    let inset = corner as usize + 1;
    let (x0, x1) = (rect.x as usize + inset, (rect.x + rect.w) as usize - inset);
    let mid = (rect.y_top + rect.h * 0.5) as usize;
    let fill = pic.at(x0, mid);
    let filled = |y: usize| far(pic.at(x0, y), fill) < 12;
    let (mut top, mut bottom) = (mid, mid);
    while top > 0 && filled(top - 1) {
        top -= 1;
    }
    while filled(bottom + 1) {
        bottom += 1;
    }
    let strongest = (top..=bottom)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .map(|(x, y)| far(pic.at(x, y), fill))
        .max()
        .unwrap_or(0);
    assert!(strongest > 60, "{label}: no label is drawn in its control");
    let ink = |x: usize, y: usize| far(pic.at(x, y), fill) * 2 >= strongest;
    let column_has_ink = |x: usize| (top..=bottom).any(|y| ink(x, y));
    let first = (x0..x1).find(|&x| column_has_ink(x)).expect("a column of ink");
    let capital = label.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let last = if capital {
        (first..x1).take_while(|&x| column_has_ink(x)).last().unwrap_or(first)
    } else {
        x1 - 1
    };
    let ink_rows: Vec<usize> = (top..=bottom).filter(|&y| (first..=last).any(|x| ink(x, y))).collect();
    Measured { rows: (top, bottom), ink: (ink_rows[0], ink_rows[ink_rows.len() - 1]) }
}

/// The frame with everything inside `rect` moved down a pixel.
fn nudged(pic: &Picture, rect: Rect, m: &Measured, corner: f64) -> Picture {
    let inset = corner as usize + 1;
    let (x0, x1) = (rect.x as usize + inset, (rect.x + rect.w) as usize - inset);
    let mut rgba = pic.rgba.clone();
    for y in (m.rows.0 + 1..=m.rows.1).rev() {
        for x in x0..x1 {
            let (to, from) = ((y * pic.w + x) * 4, ((y - 1) * pic.w + x) * 4);
            rgba.copy_within(from..from + 4, to);
        }
    }
    Picture { rgba, w: pic.w }
}

fn labels_sit_in_the_middle(lang: LangId, scale: f64) {
    let Some(frame) = capture_pixels(scale, || in_language(lang, scene)) else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let (rgba, w, _) = frame.pixels.expect("the frame's pixels");
    let pic = Picture { rgba, w: w as usize };
    let drawn = DRAWN.lock().unwrap().clone();
    assert_eq!(drawn.len(), 15, "five labels in a segment, a button and a chip each");
    for (label, kind, rect, corner) in &drawn {
        let m = measure(&pic, label, *rect, *corner);
        let off = m.off_centre();
        // half a point: one device pixel at 2x, where the system
        // font's own rounding differs by a row between OS releases
        assert!(
            off.abs() <= 0.5 * scale,
            "{lang:?} at scale {scale}: {label} in a {kind} is {off:+.1} px from the middle: the {kind} is rows {}..={}, the ink rows {}..={}",
            m.rows.0, m.rows.1, m.ink.0, m.ink.1,
        );
        // a pixel lower it is no longer in the middle, unless it sat
        // half a pixel high: then it is as far out the other way
        let lower = measure(&nudged(&pic, *rect, &m, *corner), label, *rect, *corner).off_centre();
        assert_eq!(lower, off + 1.0, "{label}: the measurement does not see the label move");
        // at 2x one device pixel is inside the allowance; the line above is the check there
        assert!(scale > 1.0 || off < 0.0 || lower.abs() > 0.5, "{label}: a label a pixel low passes");
    }
}

macro_rules! labels_sit_in_the_middle {
    ($($test:ident = $lang:expr, $scale:expr;)*) => {$(
        #[test]
        fn $test() {
            labels_sit_in_the_middle($lang, $scale);
        }
    )*};
}

labels_sit_in_the_middle! {
    labels_sit_in_the_middle_en_at_1 = LangId::En, 1.0;
    labels_sit_in_the_middle_en_at_2 = LangId::En, 2.0;
    labels_sit_in_the_middle_ja_at_1 = LangId::Ja, 1.0;
    labels_sit_in_the_middle_ja_at_2 = LangId::Ja, 2.0;
    labels_sit_in_the_middle_zh_at_1 = LangId::ZhHans, 1.0;
    labels_sit_in_the_middle_zh_at_2 = LangId::ZhHans, 2.0;
}
