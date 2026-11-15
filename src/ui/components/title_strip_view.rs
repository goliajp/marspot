//! Every pane's title strip as one tree: a floating node per pane,
//! where the layout puts the pane, and one over the clickable prefix
//! of a pane's badge.  Each takes a press itself, tested against the
//! kept layout.
//!
//! Kept apart from the pane's own instances, so output in a pane does
//! not paint its strip again, and a renamed pane or a new badge paints
//! the strips without touching any pane.

use std::hash::{Hash, Hasher};

use crate::font_cache::FontCache;
use crate::layout::Layout;
use crate::render::SessionView;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::sidebar::clip_chars;
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::chrome;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::scene::RectInstance;
use golia_ui_core::{Action, Length, Ui, WidgetId};
use marspot_term::layout::Rect;

/// What separates the title from the badge in a node's label.  The
/// badge is the plugin's and never holds it, so the last one is the
/// mark whatever a title holds.
const BADGE_MARK: char = '\u{1f}';

/// The actions: a press on pane `n`'s strip, and on the clickable
/// prefix of its badge, with `arg: n`.
pub const TITLE: u32 = 5;
pub const BADGE: u32 = 6;

/// The panes whose strips are drawn, in the layout's cell order.
pub trait Strips {
    fn count(&self) -> usize;
    fn title(&self, i: usize) -> &str;
    fn badge(&self, i: usize) -> &str;
    /// Whether pane `i` is the one in use.
    fn in_use(&self, i: usize) -> bool;
}

impl Strips for &[SessionView<'_>] {
    fn count(&self) -> usize {
        <[SessionView<'_>]>::len(self)
    }
    fn title(&self, i: usize) -> &str {
        self[i].title
    }
    fn badge(&self, i: usize) -> &str {
        self[i].right_badge
    }
    fn in_use(&self, i: usize) -> bool {
        self[i].focused
    }
}

/// The window's title strips this frame.
pub struct TitleStrips<'a> {
    pub layout: &'a Layout,
    pub strips: &'a dyn Strips,
    pub window_focused: bool,
    /// The mono cell's width, which the badge is measured in.
    pub cell_w: f64,
}

impl TitleStrips<'_> {
    fn count(&self) -> usize {
        self.strips.count().min(self.layout.cells.len())
    }

    /// Each strip's words and whether its pane is in use, folded with
    /// where the strips are.
    fn fingerprint(&self) -> u64 {
        let mut h = marspot_term::fast_hash::FxHasher::default();
        for i in 0..self.count() {
            self.strips.title(i).hash(&mut h);
            (self.strips.in_use(i) && self.window_focused).hash(&mut h);
        }
        (self.layout.gutter.to_bits(), self.hit_key()).hash(&mut h);
        h.finish()
    }

    /// What a press is tested against: where each strip is, and where
    /// its badge's prefix is.
    fn hit_key(&self) -> u64 {
        let mut h = marspot_term::fast_hash::FxHasher::default();
        let l = self.layout;
        for (i, rect) in l.cells.iter().take(self.count()).enumerate() {
            self.strips.badge(i).hash(&mut h);
            [rect.x.to_bits(), rect.y_top.to_bits(), rect.w.to_bits()].hash(&mut h);
        }
        (self.count(), l.cell_title_h.to_bits(), l.padding.to_bits(), self.cell_w.to_bits()).hash(&mut h);
        (l.window_w.to_bits(), l.window_h.to_bits()).hash(&mut h);
        h.finish()
    }
}

/// Where the clickable prefix of `badge` lies in a strip `rect` wide,
/// as `(x, w)`: what comes before its first space, the badge set flush
/// right at one cell a character.  `None` with nothing to click.
fn badge_prefix(rect: &marspot_term::layout::CellRect, padding: f64, badge: &str, cell_w: f64) -> Option<(f64, f64)> {
    let prefix = badge.split(' ').next().filter(|p| !p.is_empty())?;
    let x = rect.x + rect.w - padding - badge.chars().count() as f64 * cell_w;
    Some((x, prefix.chars().count() as f64 * cell_w))
}

/// `n` panes with nothing to show but their badges, for laying the
/// strips out where only where they are matters.
pub struct BadgesOnly<'a, F: Fn(usize) -> &'a str>(pub usize, pub F);

impl<'a, F: Fn(usize) -> &'a str> Strips for BadgesOnly<'a, F> {
    fn count(&self) -> usize {
        self.0
    }
    fn title(&self, _: usize) -> &str {
        ""
    }
    fn badge(&self, i: usize) -> &str {
        (self.1)(i)
    }
    fn in_use(&self, _: usize) -> bool {
        false
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
        for (i, rect) in l.cells.iter().take(s.count()).enumerate() {
            let (title, badge) = (s.strips.title(i), s.strips.badge(i));
            let press = (WidgetId(2 * i as u32), Action::new(TITLE, i as u32));
            // a strip with no title draws nothing and still takes a press
            ui.float(rect.x, rect.y_top, |ui| {
                let strip = ui.leaf(0, Length::Px(rect.w), Length::Px(l.cell_title_h)).pressable(press.0, press.1);
                if !title.is_empty() {
                    let tag = if s.strips.in_use(i) && s.window_focused { preset::TITLE_STRIP_IN_USE } else { preset::TITLE_STRIP };
                    strip.leaf(tag, measures).label_with(|text| {
                        text.push_str(title);
                        text.push(BADGE_MARK);
                        text.push_str(badge);
                    });
                }
            });
            if let Some((x, w)) = badge_prefix(rect, l.padding, badge, s.cell_w) {
                ui.float(x, rect.y_top, |ui| {
                    ui.leaf(0, Length::Px(w), Length::Px(l.cell_title_h))
                        .pressable(WidgetId(2 * i as u32 + 1), Action::new(BADGE, i as u32));
                });
            }
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
    hit_key: Option<u64>,
    data_rev: u64,
}

impl TitleStripSurface {
    fn place(&mut self, s: &TitleStrips<'_>, window: (f64, f64)) -> Place {
        let now = Some((s.fingerprint(), window));
        if self.described != now {
            self.described = now;
            self.hit_key = Some(s.hit_key());
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Add the strips to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, s: &TitleStrips<'_>) {
        let at = self.place(s, (p.window_w, p.window_h));
        self.kept.frame(p, at, None, |ui, _| describe(ui, s));
    }

    /// Lay the strips out again if they or a badge's prefix have moved
    /// since they were last described, so a press that arrives before
    /// the next frame is tested against where that frame will draw
    /// them.  `strips` need only say the badges right: that frame
    /// describes the rest again.
    pub fn ready(&mut self, font: &mut FontCache, layout: &Layout, strips: &dyn Strips) {
        let s = TitleStrips { layout, strips, window_focused: false, cell_w: font.cell_w };
        if self.hit_key == Some(s.hit_key()) {
            return;
        }
        let at = self.place(&s, (layout.window_w, layout.window_h));
        self.kept.lay_out(font, at, |ui, _| describe(ui, &s));
    }
}
