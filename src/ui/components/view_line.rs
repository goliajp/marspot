//! The line in the header of a window with one pane that says what the
//! pane shows while it shows a helper in its place.
//!
//! A window with two panes or more says it in the viewing pane's title
//! strip.  A lone pane has no strip, and giving it one would take a row
//! from the terminal, so the line goes on the header's own row instead:
//! between the toolbar and the version label, centred over the pane
//! where it fits.  Nothing below the header moves.

use crate::layout::Layout;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::toolbar_view::Toolbar;
use crate::ui::core::ViewPainter;
use crate::ui::core::text_fit::{cells, fit_mono_len};
use crate::ui::theme::spec::chrome;

/// Where the line is drawn: its x and baseline, and how many bytes of
/// it there is room for, with an ellipsis after them when not all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub x: f32,
    pub baseline: f32,
    pub bytes: usize,
    pub clipped: bool,
}

/// Where `text` goes in a window laid out as `layout`, with text in
/// cells `cell_w × cell_h` whose ascent is `ascent`.  `None` when
/// there is nothing to say, no header to say it in, or the panes have
/// title strips of their own.
pub fn place(layout: &Layout, text: &str, (cell_w, cell_h, ascent): (f32, f32, f32)) -> Option<Placed> {
    if text.is_empty() || layout.top_inset <= 0.0 || layout.cell_title_h > 0.0 || cell_w <= 0.0 {
        return None;
    }
    let pane = layout.cells.first()?;
    let gap = chrome::px(chrome::TITLE_GAP);
    let buttons = layout.toolbar_buttons();
    let left = buttons.iter().map(|b| b.x + b.w).fold(0.0, f64::max) + gap;
    let version = Toolbar::of(layout, [false; 5], (cell_w, cell_h, ascent)).label;
    let right = version.map_or(layout.window_w, |(x, _)| x as f64) - gap;
    let room = right - left;
    if room < cell_w as f64 {
        return None;
    }
    let (bytes, clipped) = fit_mono_len(text, room, cell_w as f64);
    let shown = cells(&text[..bytes]) + usize::from(clipped);
    let w = shown as f64 * cell_w as f64;
    let centred = pane.x + (pane.w - w) * 0.5;
    let x = centred.clamp(left, (right - w).max(left));
    // the header's one row, as the version label is set on it
    let scale = (layout.top_inset / crate::HEADER_PT) as f32;
    let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32 * scale;
    let baseline = (row_center - cell_h * 0.5).max(0.0) + ascent;
    Some(Placed { x: x as f32, baseline, bytes, clipped })
}

/// What a lone pane shows in its place, kept for the header from frame
/// to frame; empty when it shows itself.
#[derive(Default)]
pub struct ViewLine(String);

impl ViewLine {
    /// Keep `text`; whether it is new.
    pub(crate) fn set(&mut self, text: &str) -> bool {
        if self.0 == text {
            return false;
        }
        self.0.clear();
        self.0.push_str(text);
        true
    }

    pub(crate) fn paint(&self, p: &mut ViewPainter<'_>, layout: &Layout) {
        paint(p, layout, &self.0);
    }
}

/// Draw `text` where [`place`] puts it.
pub(crate) fn paint(p: &mut ViewPainter<'_>, layout: &Layout, text: &str) {
    let Some(at) = place(layout, text, (p.cell_w, p.cell_h, p.ascent)) else { return };
    let ink = palette::fg();
    let head = &text[..at.bytes];
    p.text(at.x, at.baseline, head, ink);
    if at.clipped {
        p.text(at.x + cells(head) as f32 * p.cell_w, at.baseline, "…", ink);
    }
}
