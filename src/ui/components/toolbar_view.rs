//! The toolbar and the version label as a tree: one floating node per
//! button, where the layout puts it, and one for the label.
//!
//! Each button sits where the layout puts it (`Layout::toolbar_buttons`)
//! and answers a press itself: a press and the pointer are tested
//! against the kept layout.  The tree keeps what the row last painted;
//! a button opening or closing, or the window changing, describes it
//! again, and a button lighting under the pointer only paints it again.

use crate::font_cache::FontCache;
use crate::layout::Layout;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::{Button, ButtonStyle, IconPosition};
use crate::ui::core::ViewPainter;
use crate::ui::system::macos::icons::Symbol;
use crate::ui::theme::spec::chrome;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::{Action, Length, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;

/// The toolbar's buttons, in `Layout::toolbar_buttons` order.
const SYMBOLS: [Symbol; 5] = [Symbol::Sidebar, Symbol::Layout, Symbol::Processes, Symbol::Usage, Symbol::Settings];

/// `data[1]` of a button: its panel is open.
const ON: u32 = 1;

/// The action of toolbar button `n`: `Action { id: PRESS, arg: n }`.
pub const PRESS: u32 = 1;

/// What the toolbar shows: where its buttons are, which are on, and
/// where the version label goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toolbar {
    pub buttons: [Rect; 5],
    pub on: [bool; 5],
    /// The label's x and baseline, as the painter takes them; `None`
    /// in a window with no header.
    pub label: Option<(f32, f32)>,
}

impl Toolbar {
    /// The toolbar of a window laid out as `layout`, with text in
    /// cells `cell_w × cell_h` whose ascent is `ascent`.
    pub fn of(layout: &Layout, on: [bool; 5], cell: (f32, f32, f32)) -> Self {
        let (cell_w, cell_h, ascent) = cell;
        // quiet metadata flush right, on the row of the traffic lights
        // and the buttons
        let label = (layout.top_inset > 0.0).then(|| {
            let text_w = version_label().chars().count() as f32 * cell_w;
            let scale = (layout.top_inset / crate::HEADER_PT) as f32;
            // `scale` is pixels per window unit, two to the point
            let x = (layout.window_w as f32 - (2.0 * chrome::VERSION_PAD_RIGHT) as f32 * scale - text_w).max(cell_w);
            let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32 * scale;
            (x, (row_center - cell_h * 0.5).max(0.0) + ascent)
        });
        Self { buttons: layout.toolbar_buttons(), on, label }
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
            let state = if t.on[i] { ON } else { 0 };
            ui.float(rect.x, rect.y_top, |ui| {
                ui.leaf(preset::TOOLBAR_SLOT, Length::Px(rect.w), Length::Px(rect.h))
                    .leaf(preset::TOOLBAR_SLOT, [i as u32, state])
                    .pressable(WidgetId(i as u32), Action::new(PRESS, i as u32));
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
pub(crate) fn paint_button(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2], state: WidgetState) {
    Button {
        rect,
        label: None,
        icon: Some(SYMBOLS[data[0] as usize]),
        icon_position: IconPosition::Only,
        hovered: state.hovered,
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
    /// The generation of the last layout input found the buttons in
    /// the places they were described in.
    ready_for: u64,
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

    /// Lay the toolbar of a window laid out as `layout` out again if
    /// its buttons have moved since it was last described, so input
    /// that arrives before the next frame is tested against where the
    /// next frame will draw them.  The buttons keep the states they
    /// were last drawn with until that frame.
    pub fn ready(&mut self, font: &mut FontCache, layout: &Layout) {
        if self.ready_for == layout.generation {
            return;
        }
        self.ready_for = layout.generation;
        let window = (layout.window_w, layout.window_h);
        // what a press and the pointer are tested against: where the
        // buttons are, in a window this size
        if self.described.is_some_and(|(t, w)| t.buttons == layout.toolbar_buttons() && w == window) {
            return;
        }
        let on = self.described.map_or([false; 5], |(t, _)| t.on);
        let t = Toolbar::of(layout, on, (font.cell_w as f32, font.cell_h as f32, font.ascent as f32));
        let at = self.place(&t, window);
        self.kept.lay_out(font, at, |ui, _| describe(ui, &t));
    }
}
