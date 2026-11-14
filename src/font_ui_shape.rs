//! Interface text shaped at a size and weight, through the font
//! cache's shape cache.

use super::FontCache;

impl FontCache {
    /// Phase 10c — shape at an explicit pt size, not just the
    /// startup-time `UI_FONT_POINT`.  Lets the view system request
    /// "SF Pro Bold at 28pt" for headers, "Body at 16pt" for demos,
    /// etc.;  the chrome path used to be locked to 13pt because
    /// `shape_ui_weighted_opts` always read `base_font.pt_size()`
    /// from the startup-interned font, ignoring per-run requested
    /// sizes.  Each `(weight, size_pt)` pair gets its own font_id +
    /// atlas slot via `intern_ui_weighted_at_size`.
    pub fn shape_ui_weighted_opts_at_size(
        &mut self,
        text: &str,
        weight_q: u16,
        opts: crate::font_shape::ShapeOptions,
        size_pt: f64,
    ) -> Vec<crate::font_shape::ShapedGlyph> {
        match self.shape_ui_kept(text, weight_q, opts, size_pt) {
            Some(at) => self.shaped_ui(text, at, opts).to_vec(),
            None => Vec::new(),
        }
    }

    /// [`Self::shape_ui_weighted_opts_at_size`] without the copy: the
    /// run is shaped into the cache if it is not there, and what comes
    /// back names where it is for [`Self::shaped_ui`] to read.  `None`
    /// when there is nothing to draw.
    pub fn shape_ui_kept(
        &mut self,
        text: &str,
        weight_q: u16,
        opts: crate::font_shape::ShapeOptions,
        size_pt: f64,
    ) -> Option<(u32, u16)> {
        if self.ui_font_idx == 0 || text.is_empty() {
            return None;
        }
        let base_idx = self.intern_ui_weighted_at_size(weight_q, size_pt).unwrap_or(self.ui_font_idx);
        let base_font = self.fonts.fonts.get(base_idx)?;
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(base_font.pt_size());
        let ui_id = base_idx as u32;
        let mut cache = std::mem::take(&mut self.shape_cache);
        let shaper = &self.shaper;
        cache.shape_with(text, ui_id, size_q, opts, |t, o| shaper.shape(t, ui_id, o));
        self.shape_cache = cache;
        self.fonts.sync();
        Some((ui_id, size_q))
    }

    /// The glyphs [`Self::shape_ui_kept`] shaped for `text`.
    pub fn shaped_ui(
        &self,
        text: &str,
        at: (u32, u16),
        opts: crate::font_shape::ShapeOptions,
    ) -> &[crate::font_shape::ShapedGlyph] {
        self.shape_cache.shaped(text, at.0, at.1, opts)
    }
}
