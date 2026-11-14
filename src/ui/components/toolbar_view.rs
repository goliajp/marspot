//! The toolbar and the version label as a tree: one floating node per
//! button, where the layout puts it, and one for the label.
//!
//! Where each button is stays the layout's (`Layout::toolbar_buttons`),
//! because a press is still tested against it there; the tree keeps
//! what the row last painted and paints it again only when a button
//! opens, closes or lights, or the window changes.

use crate::font_cache::FontCache;
use crate::layout::Layout;
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::{Button, ButtonStyle, IconPosition};
use crate::ui::core::ViewPainter;
use crate::ui::system::macos::icons::Symbol;
use crate::ui::theme::spec::chrome;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::{Length, Ui};
use marspot_term::layout::Rect;

/// The toolbar's buttons, in `Layout::toolbar_buttons` order.
const SYMBOLS: [Symbol; 5] = [Symbol::Sidebar, Symbol::Layout, Symbol::Processes, Symbol::Usage, Symbol::Settings];

/// `data[1]` of a button: its panel is open.
const ON: u32 = 1;
/// `data[1]` of a button: the pointer is on it.
const LIT: u32 = 2;

/// What the toolbar shows: where its buttons are, which are on and
/// which is lit, and where the version label goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toolbar {
    pub buttons: [Rect; 5],
    pub on: [bool; 5],
    pub hover: Option<ChromeHover>,
    /// The label's x and baseline, as the painter takes them; `None`
    /// in a window with no header.
    pub label: Option<(f32, f32)>,
}

impl Toolbar {
    /// The toolbar of a window laid out as `layout`, with text in
    /// cells `cell_w × cell_h` whose ascent is `ascent`.
    pub fn of(layout: &Layout, on: [bool; 5], hover: Option<ChromeHover>, cell: (f32, f32, f32)) -> Self {
        let (cell_w, cell_h, ascent) = cell;
        // quiet metadata flush right, on the row of the traffic lights
        // and the buttons
        let label = (layout.top_inset > 0.0).then(|| {
            let text_w = version_label().chars().count() as f32 * cell_w;
            let scale = (layout.top_inset / crate::HEADER_PT) as f32;
            let x = (layout.window_w as f32 - chrome::VERSION_PAD_RIGHT as f32 * scale - text_w).max(cell_w);
            let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32 * scale;
            (x, (row_center - cell_h * 0.5).max(0.0) + ascent)
        });
        Self { buttons: layout.toolbar_buttons(), on, hover, label }
    }
}

/// The running binary's version, e.g. `0.12.67 (core 0.12.494)`, made
/// once.  Bumped on every commit that changes a binary, so it is the
/// visible proof that a silent update landed.
pub fn version_label() -> &'static str {
    static LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LABEL.get_or_init(crate::version_line)
}

pub fn describe(ui: &mut Ui<'_>, t: &Toolbar) {
    ui.vstack(|ui| {
        for (i, rect) in t.buttons.iter().enumerate() {
            // a zero-width rect is a button this layout does not show
            if rect.w <= 0.0 {
                continue;
            }
            let lit = t.hover == Some(ChromeHover::Toolbar(i as u8));
            let state = if t.on[i] { ON } else { 0 } | if lit { LIT } else { 0 };
            ui.float(rect.x, rect.y_top, |ui| {
                ui.leaf(preset::TOOLBAR_SLOT, Length::Px(rect.w), Length::Px(rect.h))
                    .leaf(preset::TOOLBAR_SLOT, [i as u32, state]);
            });
        }
        if let Some((x, baseline)) = t.label {
            ui.float(0.0, 0.0, |ui| {
                ui.leaf(preset::VERSION, Length::ZERO, Length::ZERO).leaf(preset::VERSION, [x.to_bits(), baseline.to_bits()]);
            });
        }
    });
}

/// A toolbar button: its symbol on the fill its state calls for.
pub(crate) fn paint_button(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2]) {
    Button {
        rect,
        label: None,
        icon: Some(SYMBOLS[data[0] as usize]),
        icon_position: IconPosition::Only,
        hovered: data[1] & LIT != 0,
        style: ButtonStyle::toolbar(data[1] & ON != 0),
    }
    .paint(p);
}

/// The version label, at the x and baseline the node carries.
pub(crate) fn paint_version(p: &mut ViewPainter<'_>, data: [u32; 2]) {
    p.text(f32::from_bits(data[0]), f32::from_bits(data[1]), version_label(), palette::chrome_icon());
}

/// The toolbar's surface: its tree, its layout and the instances it
/// last painted.
#[derive(Default)]
pub struct ToolbarSurface {
    pub kept: KeptSurface,
    described: Option<(Toolbar, (f64, f64))>,
    data_rev: u64,
}

impl ToolbarSurface {
    fn place(&mut self, t: &Toolbar, window: (f64, f64)) -> Place {
        if self.described != Some((*t, window)) {
            self.described = Some((*t, window));
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Add the toolbar to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, t: &Toolbar) {
        let at = self.place(t, (p.window_w, p.window_h));
        self.kept.frame(p, at, None, |ui, _| describe(ui, t));
    }

    /// Lay the toolbar out without painting.
    pub fn lay_out(&mut self, font: &mut FontCache, t: &Toolbar, window: (f64, f64)) {
        let at = self.place(t, window);
        self.kept.lay_out(font, at, |ui, _| describe(ui, t));
    }
}
