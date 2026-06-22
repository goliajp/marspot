//! Dev panel — the formal workbench for marspot's UI system.
//!
//! Built with the new Canvas API (no `ViewPainter`).  Renders
//! through `encode_canvas` in the same z-bracket the context
//! menu uses, so primitives sit above the grid but below the
//! transient context menu.
//!
//! Subsequent commits will add:
//! - tab strip + tab content (Tokens / Primitives / Components)
//! - toolbar button toggle + Cmd-shortcut
//! - drag-to-move + resize handles
//! - persistence to `dev-panel-state.bin`
//!
//! This first commit is the skeleton:  a positioned, sized
//! Canvas-rendered frame with a title bar.  Toggling is wired
//! via a debug keyboard shortcut.

use crate::ui::core::{Canvas, Length, ParentRect, Pt};

/// Live state for the dev panel.  Owned by `CoreApp` /
/// `MarspotApp`.  Default constructor returns a centred,
/// open panel — the user's first launch shows it.
///
/// `scale` is the device pixel ratio at publish time; CoreApp
/// stamps it in when handing the state to the renderer.
#[derive(Clone, Debug)]
pub struct DevPanelState {
    /// `true` when the panel is rendered + accepts mouse input.
    pub visible: bool,
    /// Position of the panel's top-left in logical points
    /// (`backingScaleFactor`-independent).  Origin is the
    /// window's top-left.
    pub origin_pt: (f64, f64),
    /// Size in logical points.
    pub size_pt: (f64, f64),
    /// Active tab index.  Tabs themselves are defined elsewhere;
    /// this just remembers which one is selected so the panel
    /// reopens to the same view.
    pub active_tab: usize,
    /// Device pixel ratio.  Defaults to 2.0; the renderer side
    /// overwrites it from the live NSWindow each frame so persisted
    /// state across displays stays correct.
    pub scale: f64,
}

impl Default for DevPanelState {
    fn default() -> Self {
        Self {
            visible: true,
            // Sized for a reasonable workbench area.  Centring
            // happens at render time so the panel stays roughly
            // mid-window as the window resizes — the persisted
            // origin (when persistence lands) overrides this.
            origin_pt: (60.0, 80.0),
            size_pt: (420.0, 520.0),
            active_tab: 0,
            scale: 2.0,
        }
    }
}

/// Tokens used by the dev panel chrome.  Lifted out of inline
/// `Color::rgba(…)` calls so the upcoming Tokens tab can preview
/// them by name.  Once P3 adds a real theme module these move
/// there; for now they live here.
pub mod tokens {
    use crate::ui::core::Color;

    pub const PANEL_BG:      Color = Color::rgba(20, 22, 28, 0.96);
    pub const PANEL_BORDER:  Color = Color::rgba(56, 60, 70, 1.0);
    pub const TITLE_BAR_BG:  Color = Color::rgba(35, 38, 46, 1.0);
    pub const TITLE_FG:      Color = Color::rgba(217, 224, 235, 1.0);
    pub const SHADOW:        Color = Color::rgba(0, 0, 0, 0.55);
    pub const DIVIDER:       Color = Color::rgba(255, 255, 255, 0.10);
}

/// Build a Canvas of dev-panel primitives.  Caller (renderer)
/// flushes through `encode_canvas_into` after the main passes.
///
/// `chrome_cell_w` / `chrome_cell_h` are the renderer's chrome-
/// font cell metrics; the title-bar text positioning needs them
/// (consistent with other Canvas-based components).
pub fn build_dev_panel_canvas(
    state: &DevPanelState,
    window_w_phys: f64,
    window_h_phys: f64,
    chrome_cell_w: f32,
    chrome_cell_h: f32,
) -> Canvas {
    let scale = state.scale;
    let mut canvas = Canvas::new(scale, ParentRect::window(window_w_phys, window_h_phys));

    // Dev panel now lives in its own NSWindow — the window itself
    // provides title bar, close button, drag, rounded corners,
    // shadow.  Our canvas paints the CONTENT area only.  No
    // internal frame / title bar duplication.
    let _ = (state.origin_pt, state.size_pt, chrome_cell_w, chrome_cell_h);

    // Solid BG covering the full window content.  Pct(1.0) =
    // 100 % of parent so it auto-resizes with the NSWindow.
    canvas.rect()
        .at(Length::Pt(0.0), Length::Pt(0.0))
        .size(Length::Pct(1.0), Length::Pct(1.0))
        .fill(tokens::PANEL_BG)
        .draw();

    // Placeholder content — replaced by tab strip + tab body in
    // P3 commits.  Single text line tells the user the panel is
    // alive and tells the next dev (you, in 30 minutes) what to
    // build next.
    canvas.text(
        Length::Pt(16.0),
        Length::Pt(16.0),
        "Dev Panel — tab strip / token gallery coming next",
    )
    .color(tokens::TITLE_FG)
    .draw();

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::core::Primitive;

    #[test]
    fn default_state_is_visible_and_centred_ish() {
        let s = DevPanelState::default();
        assert!(s.visible);
        assert_eq!(s.active_tab, 0);
        assert!(s.size_pt.0 > 200.0);
        assert!(s.size_pt.1 > 200.0);
    }

    #[test]
    fn canvas_emits_bg_then_placeholder_text() {
        let s = DevPanelState::default();
        let c = build_dev_panel_canvas(&s, 1920.0, 1080.0, 16.0, 32.0);
        let prims = c.primitives();
        assert_eq!(prims.len(), 2);
        match &prims[0] { Primitive::Rect(_) => (), _ => panic!("expected BG rect") }
        match &prims[1] {
            Primitive::Text(t) => assert!(t.content.starts_with("Dev Panel")),
            _ => panic!("expected placeholder text"),
        }
    }

    #[test]
    fn hidden_state_still_builds_a_canvas() {
        // The renderer guards on `state.visible` before calling, so
        // an invisible state never reaches this function.  But the
        // builder mustn't panic when called — defensive lower bound
        // for the input space.
        let s = DevPanelState { visible: false, ..Default::default() };
        let c = build_dev_panel_canvas(&s, 800.0, 600.0, 8.0, 16.0);
        assert!(c.len() > 0);
    }
}
