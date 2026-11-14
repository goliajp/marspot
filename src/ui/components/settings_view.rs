//! The settings panel as a tree: the panel, its groups, each a card of
//! rows, and the footer.
//!
//! What a row is and what a press on it changes are
//! `settings_rows`'; this file only says where things go.  A press or
//! a key on a control comes back as [`CHOOSE`] with the row and the
//! segment packed in its argument.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::font_cache::FontCache;
use crate::settings::Settings;
use crate::ui::core::ViewPainter;
use crate::ui::components::controls::Measure;
use crate::ui::components::settings_modal::{self as sm, Control, Row, metric};
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, card, content, space};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{FontMeasure, KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{Action, AlignCross, Edges, Length, Ui, WidgetId};
use marspot_term::layout::Rect;

/// The scroll slot the panel's content is in.
pub const SCROLL: u8 = 0;
/// A control was pressed: the argument is [`choice`].
pub const CHOOSE: u32 = 1;

/// Row `row`, segment `seg` (0 for a toggle), as an action's argument.
pub fn choice(row: usize, seg: usize) -> u32 {
    ((row as u32) << 8) | seg as u32
}

/// The row and segment an argument made by [`choice`] names.
pub fn chosen(arg: u32) -> Option<(Row, usize)> {
    let spec = sm::rows().nth((arg >> 8) as usize)?;
    Some((spec.row, (arg & 0xff) as usize))
}

/// The panel `frame` pixels in size, showing `s`.
pub fn describe(
    ui: &mut Ui<'_>,
    fm: &FontMeasure<'_>,
    frame: (f64, f64),
    backdrop_top: f64,
    s: &Settings,
    path: &str,
) {
    let mut m = fm.for_controls();
    panel::panel(ui, frame, backdrop_top, t(crate::ui::strings::Msg::PanelSettings), None, |ui| {
        ui.scroll(SCROLL, |ui| {
            let mut index = 0;
            for (si, section) in sm::SECTIONS.iter().enumerate() {
                if si > 0 {
                    ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_HEADING));
                }
                ui.text(font_of(content::HEADING), Tone::Fg.into(), t(section.heading));
                ui.leaf(0, Length::ZERO, Length::Pt(space::HEADING_TO_CARD));
                ui.vstack(|ui| {
                    for (ri, row_spec) in section.rows.iter().enumerate() {
                        if ri > 0 {
                            ui.rule(Length::Px(card::SEPARATOR_PX), Tone::Separator.into())
                                .padding(Edges::xy(Length::Pt(card::SEPARATOR_INSET), Length::ZERO));
                        }
                        row(ui, &mut m, index, row_spec, s);
                        index += 1;
                    }
                })
                .width(Length::Pct(1.0))
                .fill(Tone::CardBg.into())
                .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
                .radius(Length::Pt(card::RADIUS));
            }
            ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_FOOTER));
            ui.text_copy(font_of(metric::FOOTER), Tone::FgFaint.into(), path);
        })
        .width(Length::Pct(1.0));
    });
}

/// One row: the label and the control on one line, what the setting
/// costs under them.
fn row(ui: &mut Ui<'_>, m: Measure<'_>, index: usize, row_spec: &'static sm::RowSpec, s: &Settings) {
    let row = row_spec.row;
    let dim = row.disabled_by(s);
    let (fg, sec) = if dim { (Tone::FgFaint, Tone::FgFaint) } else { (Tone::Fg, Tone::FgMuted) };
    let cost = if row == Row::Language { sm::language_cost() } else { row_spec.cost };
    let id = WidgetId(index as u32);
    ui.vstack(|ui| {
        ui.hstack(|ui| {
            ui.text(font_of(metric::LABEL), fg.into(), t(row_spec.label));
            ui.spacer();
            match row.control(s) {
                Control::Toggle(on) => {
                    preset::toggle(ui, id, Action::new(CHOOSE, choice(index, 0)), on, dim)
                }
                Control::Segmented { options, chosen } => {
                    // out of range when the file holds a value the
                    // panel has no segment for
                    let chosen = (chosen < options.len()).then_some(chosen);
                    preset::segmented_with(ui, m, id, options, chosen, dim, |n| {
                        Action::new(CHOOSE, choice(index, n))
                    });
                }
            }
        })
        .height(Length::Pt(metric::ROW_BAND_H))
        .align_cross(AlignCross::Center);
        ui.text(font_of(metric::DESC), sec.into(), t(cost));
    })
    .width(Length::Pct(1.0))
    .height(Length::Px(spec::px(sm::row_h_pt())))
    .padding(Edges::xy(Length::Pt(card::ROW_PAD_X), Length::Pt(card::ROW_PAD_Y)))
    .gap(Length::Pt(space::LABEL_TO_DESC))
    .whole();
}

/// The settings panel's render data: where it is, the values in force
/// when the frame was built, and the surface it is drawn through.
///
/// The surface is shared with whoever takes the panel's input: the
/// layout a press is tested against is the one the frame was painted
/// from.
#[derive(Clone)]
pub struct SettingsRender {
    pub rect: Rect,
    /// How far the content is pushed up inside `rect`.  Nonzero only
    /// when the window is too short to show the panel whole.
    pub scroll: f64,
    pub settings: Arc<Settings>,
    /// Shown in the footer so the file is findable.
    pub path: Arc<str>,
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: Rc<RefCell<SettingsSurface>>,
}

impl SettingsRender {
    /// A panel at `rect` on a surface of its own.
    pub fn new(rect: Rect, scroll: f64, settings: Settings, path: &str) -> Self {
        Self {
            rect,
            scroll,
            settings: Arc::new(settings),
            path: Arc::from(path),
            pointer: None,
            surface: Default::default(),
        }
    }
}

/// What the panel was last described from.
struct Described {
    settings: Arc<Settings>,
    path: Arc<str>,
    size: (f64, f64),
    backdrop_top: f64,
}

/// The settings panel's surface: its tree, its layout, what the
/// pointer and the keyboard have done to it, and the instances it last
/// painted.
#[derive(Default)]
pub struct SettingsSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
}

impl SettingsSurface {
    /// Where `sp` puts the surface, having counted a change in what it
    /// shows since it was last described.
    fn place(&mut self, sp: &SettingsRender, backdrop_top: f64) -> Place {
        let size = (sp.rect.w, sp.rect.h);
        let same = self.described.as_ref().is_some_and(|d| {
            (Arc::ptr_eq(&d.settings, &sp.settings) || d.settings == sp.settings)
                && d.path == sp.path
                && d.size == size
                && d.backdrop_top == backdrop_top
        });
        if !same {
            self.described =
                Some(Described { settings: sp.settings.clone(), path: sp.path.clone(), size, backdrop_top });
            self.data_rev += 1;
        }
        self.kept.surface.interaction.set_scroll(SCROLL as usize, sp.scroll);
        Place { data_rev: self.data_rev, origin: (sp.rect.x, sp.rect.y_top), avail: size }
    }

    /// Bring the tree and its layout up to `sp` without painting, so
    /// that a press, a key or the pointer is tested against the panel
    /// as the next frame will show it.
    pub fn lay_out(&mut self, font: &mut FontCache, sp: &SettingsRender, backdrop_top: f64) {
        let at = self.place(sp, backdrop_top);
        self.kept.lay_out(font, at, |ui, fm| describe(ui, fm, at.avail, backdrop_top, &sp.settings, &sp.path));
    }

    /// Add the panel to the frame `p` is painting.  It is described
    /// again only when what it shows is not what it was described
    /// from.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, sp: &SettingsRender, backdrop_top: f64) {
        let at = self.place(sp, backdrop_top);
        self.kept.frame(p, at, sp.pointer, |ui, fm| {
            describe(ui, fm, at.avail, backdrop_top, &sp.settings, &sp.path)
        });
    }
}
