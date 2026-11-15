//! Every pane's title strip as one tree: a floating node per pane with
//! a title, where the layout puts the pane.
//!
//! Kept apart from the pane's own instances, so output in a pane does
//! not paint its strip again, and a renamed pane or a new badge paints
//! the strips without touching any pane.

use std::hash::{Hash, Hasher};

use crate::layout::Layout;
use crate::render::SessionView;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::sidebar::clip_chars;
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::chrome;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::scene::RectInstance;
use golia_ui_core::{Length, Ui};
use marspot_term::layout::Rect;

/// What separates the title from the badge in a node's label.  The
/// badge is the plugin's and never holds it, so the last one is the
/// mark whatever a title holds.
const BADGE_MARK: char = '\u{1f}';

/// The window's title strips this frame.
pub struct TitleStrips<'a> {
    pub layout: &'a Layout,
    pub views: &'a [SessionView<'a>],
    pub window_focused: bool,
}

impl TitleStrips<'_> {
    /// Each strip's place, words and whether its pane is in use, folded
    /// with the layout's measures.
    fn fingerprint(&self) -> u64 {
        let mut h = marspot_term::fast_hash::FxHasher::default();
        let l = self.layout;
        for (view, rect) in self.views.iter().zip(&l.cells) {
            view.title.hash(&mut h);
            view.right_badge.hash(&mut h);
            (view.focused && self.window_focused).hash(&mut h);
            [rect.x.to_bits(), rect.y_top.to_bits(), rect.w.to_bits()].hash(&mut h);
        }
        (self.views.len(), l.cell_title_h.to_bits(), l.gutter.to_bits(), l.padding.to_bits()).hash(&mut h);
        h.finish()
    }
}

pub fn describe(ui: &mut Ui<'_>, s: &TitleStrips<'_>) {
    let l = s.layout;
    let title_h = l.cell_title_h as f32;
    let measures = [(l.padding as f32).to_bits(), (l.gutter as f32).to_bits()];
    ui.vstack(|ui| {
        if title_h <= 0.0 {
            return;
        }
        for (view, rect) in s.views.iter().zip(&l.cells) {
            if view.title.is_empty() {
                continue;
            }
            let tag = if view.focused && s.window_focused { preset::TITLE_STRIP_IN_USE } else { preset::TITLE_STRIP };
            ui.float(rect.x, rect.y_top, |ui| {
                ui.leaf(tag, Length::Px(rect.w), Length::Px(l.cell_title_h)).leaf(tag, measures).label_with(|text| {
                    text.push_str(view.title);
                    text.push(BADGE_MARK);
                    text.push_str(view.right_badge);
                });
            });
        }
    });
}

/// One pane's strip, in the rect of its node: the seam along its
/// bottom, the name, and the badge with a line under its clickable
/// prefix.  At the unrounded cell metrics, as it always was.
pub(crate) fn paint(p: &mut ViewPainter<'_>, rect: Rect, label: &str, data: [u32; 2], in_use: bool) {
    let (title, badge) = label.rsplit_once(BADGE_MARK).unwrap_or((label, ""));
    let (padding, gutter) = (f32::from_bits(data[0]), f32::from_bits(data[1]));
    let (cell_w, cell_h, ascent) = (p.cell_w, p.cell_h, p.ascent);
    let title_h = rect.h as f32;
    // The strip sits flush with the pane's background; one seam along
    // its bottom separates it from the terminal below.  The name is
    // set as a sidebar row's is: primary on the pane in use, a step
    // back on the others.
    let strip_bottom = rect.y_top as f32 + title_h;
    p.cells.push(RectInstance {
        origin: [rect.x as f32, strip_bottom - gutter.max(1.0)],
        size: [rect.w as f32, gutter.max(1.0)],
        color: crate::frame_build::color::rgba8_of_f32(palette::seam()),
    });
    // the terminal's own mono metrics, centred in the strip, from the
    // terminal's own inset
    let label_x = rect.x as f32 + padding;
    let baseline = rect.y_top as f32 + (title_h - cell_h) * 0.5 + ascent;
    let badge_x = rect.x as f32 + rect.w as f32 - padding - crate::ui::core::text_fit::cells(badge) as f32 * cell_w;
    let badge_shown = !badge.is_empty() && badge_x > label_x;
    // the name gives way to the badge, and to the pane's edge
    let title_right = if badge_shown {
        badge_x - chrome::px(chrome::TITLE_GAP) as f32
    } else {
        rect.x as f32 + rect.w as f32 - padding
    };
    let room = ((title_right - label_x) / cell_w).floor().max(0.0) as usize;
    p.text(label_x, baseline, clip_chars(title, room), if in_use { palette::fg() } else { palette::chrome_fg() });
    if !badge_shown {
        return;
    }
    // A plugin's badge, at the right edge.  What comes before its
    // first space is the clickable prefix (the plugin owns what it
    // means) and is underlined, a line below the baseline.
    p.text(badge_x, baseline, badge, palette::badge_fg());
    let prefix_chars = badge.split(' ').next().map(|s| s.chars().count()).unwrap_or(0);
    if prefix_chars > 0 {
        p.cells.push(RectInstance {
            origin: [badge_x, baseline + gutter.max(1.0)],
            size: [prefix_chars as f32 * cell_w, gutter.max(1.0)],
            color: crate::frame_build::color::rgba8_of_f32(palette::badge_fg()),
        });
    }
}

/// The title strips' surface: its tree, its layout and the instances
/// it last painted.
#[derive(Default)]
pub struct TitleStripSurface {
    pub kept: KeptSurface,
    described: Option<(u64, (f64, f64))>,
    data_rev: u64,
}

impl TitleStripSurface {
    /// Add the strips to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, s: &TitleStrips<'_>) {
        let window = (p.window_w, p.window_h);
        let now = Some((s.fingerprint(), window));
        if self.described != now {
            self.described = now;
            self.data_rev += 1;
        }
        let at = Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window };
        self.kept.frame(p, at, None, |ui, _| describe(ui, s));
    }
}
