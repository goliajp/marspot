//! Phase 10c — bridge between the view system's
//! `FontMetricsProvider` trait and the chrome renderer's `FontCache`.
//!
//! The view layout pass needs real per-string advance + line-height
//! for both Monaco mono and SF Pro proportional text;  `FontCache`
//! has the underlying `measure_ui_text` / `ui_line_h_phys` /
//! `ui_metrics` APIs but they take `&mut self` because shape +
//! variant interning mutate caches.  This module wraps a
//! `&mut FontCache` borrow in `RefCell` and implements
//! `FontMetricsProvider` (which expects `&self`) on top — pragmatic
//! interior mutability scoped to the layout call.
//!
//! Monaco metrics come from the cell pitch the caller already
//! knows (chrome_cell_w / chrome_cell_h in physical pixels), so
//! the Mono path doesn't touch `FontCache` at all.

use crate::font_cache::FontCache;
use crate::ui::view::{FontMetricsProvider, TextFontSpec, TextSize, unpack_shape_opts};
use std::cell::RefCell;

/// Live chrome-measure provider — backed by the renderer's
/// `FontCache`.  Lifetime `'a` is the borrow on the cache; layout
/// shouldn't outlive a single render call.
pub struct ChromeMeasure<'a> {
    font: RefCell<&'a mut FontCache>,
    mono_cell_w_phys: f64,
    mono_cell_h_phys: f64,
}

impl<'a> ChromeMeasure<'a> {
    pub fn new(
        font: &'a mut FontCache,
        mono_cell_w_phys: f64,
        mono_cell_h_phys: f64,
    ) -> Self {
        Self {
            font: RefCell::new(font),
            mono_cell_w_phys,
            mono_cell_h_phys,
        }
    }
}

fn mono_size_mult(size: TextSize) -> f64 {
    match size {
        TextSize::Caption => 0.85,
        TextSize::Body => 1.00,
        TextSize::Header => 1.20,
        TextSize::LargeHeader => 1.50,
    }
}

impl<'a> FontMetricsProvider for ChromeMeasure<'a> {
    fn line_h_phys(&self, font: TextFontSpec) -> f64 {
        match font {
            TextFontSpec::Mono { size } => self.mono_cell_h_phys * mono_size_mult(size),
            TextFontSpec::Ui { .. } => {
                // SF Pro line-height — `FontCache::ui_line_h_phys`
                // bakes the 2× retina scale.  size_q is currently
                // ignored because we only intern SF Pro at one pt
                // size (UI_FONT_POINT); when Phase 5+ adds true
                // multi-size SF Pro this branch must vary.
                self.font.borrow().ui_line_h_phys()
            }
        }
    }

    fn advance_phys(&self, text: &str, font: TextFontSpec) -> f64 {
        match font {
            TextFontSpec::Mono { .. } => {
                crate::ui::view::layout::text_width_cells(text) as f64 * self.mono_cell_w_phys
            }
            TextFontSpec::Ui { weight, opts_bits, .. } => {
                let opts = unpack_shape_opts(opts_bits);
                self.font.borrow_mut().measure_ui_text(text, weight, opts)
            }
        }
    }
}
