//! The right-click context menu: the data a frame of it is drawn
//! from, and the canvas it is drawn as.  Its geometry and hit-testing
//! are in `context_menu`.

/// F3+9 — full data for one render of a right-click context menu.
/// L2 builds this from `Marspot::context_menu` on every frame the
/// menu is open; renderer paints it through overlay scratches so it
/// lands on top of the grid + sidebar.
#[derive(Debug, Clone)]
pub struct ContextMenuRender {
    pub scale: f64,
    pub anchor_phys: (f64, f64),
    pub top_inset: f64,
    /// One per row.  Drives label + shortcut + enabled / divider
    /// rendering.  Owned by the renderer-side struct (rebuilt each
    /// frame the menu is open) so the renderer doesn't need a shared
    /// reference into Marspot state.
    pub items: Vec<ContextMenuRow>,
    /// Index of the row currently under the cursor, or `None` if
    /// the cursor isn't over any actionable row.
    pub hovered_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ContextMenuRow {
    pub label: String,
    pub shortcut_hint: String,
    pub enabled: bool,
    pub divider: bool,
}

/// F3+9 / P2c — build the right-click ContextMenu as a `Canvas`.
/// Geometry from `ContextMenu::layout`; submission order = z so
/// frame BG goes first, then hover band, then divider hairline,
/// then text — last-submitted wins on top.  Caller flushes the
/// returned canvas via `MetalRenderer::encode_canvas` AFTER all
/// other overlay passes so the menu trumps everything else.
///
/// Why a separate canvas (not the shared overlay scratches):
/// the encode_passes path fixes the BG-cells-before-UI-rects
/// order, which silently buried the divider in F3+12.x.  Routing
/// the menu through its own canvas + `encode_canvas` puts every
/// primitive on a submission-order timeline regardless of which
/// pipeline carries it.
/// SF Pro's ascent as a fraction of its point size — measured from
/// the interned CTFont at startup and stable across sizes.  Used only
/// where a run has to be centred against a box whose height came from
/// somewhere else (menu rows); anything drawing into a `ViewPainter`
/// should use `ui_baseline_centred` instead, which asks the font.
const SF_PRO_ASCENT_RATIO: f64 = 0.75;

pub(crate) fn build_context_menu_canvas(
    state: &ContextMenuRender,
    window_w: f64,
    window_h: f64,
    chrome_cell_w: f32,
    chrome_cell_h: f32,
) -> crate::ui::core::Canvas {
    use crate::ui::core::{Canvas, Color, Length, Pt, ParentRect};
    use crate::ui::components::{ContextMenu, MenuItem};

    let menu_items: Vec<MenuItem> = state
        .items
        .iter()
        .map(|r| MenuItem {
            label: r.label.clone(),
            shortcut_hint: r.shortcut_hint.clone(),
            enabled: r.enabled,
            divider: r.divider,
            action_tag: 0,
        })
        .collect();
    let menu = ContextMenu::layout(
        window_w, window_h, state.scale,
        state.anchor_phys.0, state.anchor_phys.1,
        state.top_inset,
        &menu_items,
    );

    let mut canvas = Canvas::new(state.scale, ParentRect::window(window_w, window_h));

    // ── Style tokens (will move to a theme module in P3) ──
    let bg          = Color::rgba(33, 36, 43, 1.0);     // MODAL_FRAME_BG
    let border      = Color::rgba(56, 60, 70, 1.0);     // approx MODAL_FRAME_BORDER
    let shadow      = Color::rgba(0, 0, 0, 0.45);
    let label_fg    = Color::rgba(217, 224, 235, 1.0);
    let label_disab = Color::rgba(115, 122, 133, 1.0);
    let hint_fg     = Color::rgba(140, 153, 168, 1.0);
    let hover_bg    = Color::rgba(51, 107, 173, 1.0);
    // Web `border: 1px solid` semantics: 1pt thick + alpha tuned for
    // clear visibility on the menu BG.  alpha=0.5 lands the rendered
    // pixel ≈ rgb(144, 145, 149) over bg rgb(33, 36, 43) — the kind
    // of contrast Chrome / Safari show for `rgba(255,255,255,0.5)`
    // on a near-black panel.  Earlier 0.22 was a misjudgement (line
    // showed but user reported "几乎看不清").
    let divider_c   = Color::rgba(255, 255, 255, 0.50);
    let side_pad_pt = 12.0_f64;

    // Helper: phys → Pt via `/ scale` so existing layout output
    // (which is already physical px) plugs cleanly into the
    // logical Pt API.  Length::Pt(x) resolves back to `x * scale`,
    // so this round-trips bit-perfectly at every scale.
    let pt_phys = |phys: f64| Length::Pt(phys / state.scale);
    let side_pad_phys = side_pad_pt * state.scale;
    let label_w_phys  = |s: &str| s.chars().count() as f64 * chrome_cell_w as f64;

    // 1. Menu frame: BG + border + shadow.
    canvas.rect()
        .at(pt_phys(menu.frame.x), pt_phys(menu.frame.y_top))
        .size(pt_phys(menu.frame.w), pt_phys(menu.frame.h))
        .fill(bg)
        .radius(Pt(6.0))
        .border(Pt(1.0), border)
        .shadow(Pt(16.0), (Pt(0.0), Pt(0.0)), shadow)
        .draw();

    // 2. Per-row primitives.  All sub-row arithmetic done in
    // physical pixels (item rects come from ContextMenu::layout
    // pre-scaled), then `pt_phys` wraps for the builder.
    for (i, row) in state.items.iter().enumerate() {
        let item = &menu.item_rects[i];
        if row.divider {
            let cy = item.y_top + item.h * 0.5;
            canvas.line(
                (pt_phys(item.x + side_pad_phys), pt_phys(cy)),
                (pt_phys(item.x + item.w - side_pad_phys), pt_phys(cy)),
            )
            .stroke(Pt(1.0), divider_c)
            .draw();
            continue;
        }
        if state.hovered_idx == Some(i) {
            canvas.rect()
                .at(pt_phys(item.x + side_pad_phys * 0.5), pt_phys(item.y_top))
                .size(pt_phys(item.w - side_pad_phys), pt_phys(item.h))
                .fill(hover_bg)
                .radius(Pt(4.0))
                .draw();
        }
        let fg = if row.enabled { label_fg } else { label_disab };
        // Menu labels are prose — the same role every other panel sets
        // a label in.  The canvas path takes a top-of-em y and derives
        // the baseline from the run's own ascent, so the y that puts
        // the *cap* on the row's centre line is
        // `centre - (ascent - cap/2)`.
        let role = crate::ui::theme::PanelText::Item;
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(role.pt());
        let text_y_phys = item.y_top + item.h * 0.5
            - (SF_PRO_ASCENT_RATIO * role.pt() - role.cap() * 0.5) * px;
        canvas.text(
            pt_phys(item.x + side_pad_phys),
            pt_phys(text_y_phys),
            &row.label,
        ).color(fg).ui().ui_size_q(size_q).weight(role.weight()).draw();
        if !row.shortcut_hint.is_empty() {
            // The shortcut is a glyph cluster (⌘⇧W), not prose — it
            // stays mono, where the symbols keep their own width and
            // right-align cleanly.
            let hint_w = label_w_phys(&row.shortcut_hint);
            let hint_y = item.y_top + (item.h - chrome_cell_h as f64) * 0.5;
            canvas.text(
                pt_phys(item.x + item.w - side_pad_phys - hint_w),
                pt_phys(hint_y),
                &row.shortcut_hint,
            ).color(hint_fg).draw();
        }
    }

    canvas
}
