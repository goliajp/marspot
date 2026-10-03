//! Chrome / cursor / focus-outline constants, kept in sync with
//! `render.rs`.  Shaders consume `f32`s, so duplicate as `f32`-tuples
//! here rather than convert per call.

// Inter-cell divider colour — light grey so a thin "white-on-dark"
// hairline shows between panes (iTerm2 style — the working area
// === Visual design ===
//
// Two near-black tones plus one quiet seam.  The focused pane
// READS DEEPER than the rest: the surrounding chrome (sidebar,
// header, unfocused cells) sits at `BG_PANEL`, while the active
// pane drops to `BG_FOCUSED` (= `font_cache::BG`) — one shade
// below.  Focus = "the deep canvas I'm typing into", everything
// else = "the surrounding panel".  Every internal boundary uses
// one weak `SEAM` hairline at one width, so the eye reads
// structure without any seam taking on chrome weight.
//
//   BG_PANEL    the dominant surface (sidebar + header +
//               unfocused cell rects); has a faint blue tint
//   BG_FOCUSED  the focused cell rect — visibly deeper, closer
//               to pure black; this IS the focus indicator
//   SEAM        a hair darker than BG_PANEL, reads as a quiet
//               depression between adjacent panels — used for
//               sidebar↔grid, header↔grid, and cell↔cell alike
// BG_PANEL lifted in a second pass (2026-06-15) to widen the
// focused/unfocused contrast — the prior (0.022, 0.028, 0.042) was
// too close to BG_FOCUSED even after pushing focused toward true
// black.  Still a deep tone, just decisively above 0.
pub(crate) const BG_PANEL: (f32, f32, f32) = (0.040, 0.050, 0.075);
// Pure black for the focused pane.  User feedback: "focused 还得再
// 黑一点".  Snapping all-zero is fine here — we never paint
// foreground glyphs in pure white, so the BG-to-glyph contrast is
// dominated by glyph color, not a few thousandths of BG tint.
pub(crate) const BG_FOCUSED: (f32, f32, f32) = (0.000, 0.000, 0.000);
/// C4 — search-hit highlight BG.  Bright yellow with reverse foreground
/// (mid-luminance, slightly desaturated so the original glyph FG
/// reads clearly on top — distinct from text-selection's BG+FG
/// inversion).  §6.8 colour roles.
pub(crate) const HIGHLIGHT_BG: (f32, f32, f32) = (0.92, 0.78, 0.20);
// Pre-mixed against BG_PANEL ≈ 50%, so the 0.5-px sub-pixel quad
// reads as a translucent hairline.  Going through alpha blending
// would need pipeline changes; this gets the same visual effect
// at the BG-pipeline solid-fill cost.
// Bumped twice on 2026-06-15.  First bump tracked the BG_PANEL lift
// to preserve the original luminance gap; user reported the seams
// were still subtle ("可能本来就是有点淡"), so second bump pushes
// the gap further — luminance diff vs BG_PANEL ≈ 0.08, comfortably
// above the just-noticeable-difference threshold without bleeding
// into chrome territory.  Same direction (brighter than panel),
// gutter width unchanged so seams stay 1 hairline thick.
pub(crate) const SEAM: (f32, f32, f32) = (0.115, 0.130, 0.155);
/// Selected-cell highlight — a muted brand blue that lifts cleanly
/// over BG_FOCUSED without bleaching foreground text.  Used by the
/// drag-to-select machinery; FG glyphs draw on top so selected
/// content stays legible.
pub(crate) const SELECTION_BG: (f32, f32, f32) = (0.16, 0.22, 0.34);
// IME preedit colours.  BG a touch above the focused-cell BG so the
// preview stands out without screaming; FG slightly muted vs the
// committed-text FG so the user reads "in flight, not yet".  The
// hairline underline below the glyph is what most editors use to
// flag composition state.
pub(crate) const IME_PREEDIT_BG: (f32, f32, f32) = (0.10, 0.13, 0.18);
pub(crate) const IME_PREEDIT_FG: (f32, f32, f32) = (0.80, 0.86, 0.92);
// Old name retained for the existing sidebar BG drawing path
// (kept flush with the panel surface).
pub(crate) const SIDEBAR_BG_F: (f32, f32, f32) = BG_PANEL;
pub(crate) const CURSOR_FG: (f32, f32, f32) = (0.92, 0.92, 0.92);

pub(crate) const SIDEBAR_DOT_R: f32 = 4.5;
pub(crate) const SIDEBAR_LEFT_PAD: f32 = 14.0;
// Sidebar's row 0 offset is now `layout::sidebar_top_pad_phys` —
// computed by Layout::build to reserve room for the [+] header
// band.  Threaded through `push_sidebar` instead of read from a
// local const.
pub(crate) const SIDEBAR_ROW_H: f32 = 22.0;
pub(crate) const SIDEBAR_DOT_LABEL_GAP: f32 = 10.0;
pub(crate) const SIDEBAR_TEXT_FG: (f32, f32, f32) = (0.78, 0.82, 0.88);
/// Header version label — slightly brighter than sidebar metadata so
/// the "current version" reads clearly when the user glances up to
/// confirm an update landed.
pub(crate) const HEADER_VERSION_FG: (f32, f32, f32) = (0.62, 0.68, 0.80);
/// Deferred-update refresh glyph in the focused pane's title strip — a
/// warm amber so it reads as an actionable "update ready" control against
/// the dim title text (target #4 step 5b).
pub(crate) const REFRESH_ICON_FG: (f32, f32, f32) = (0.95, 0.74, 0.30);
/// Claudecode brand coral — matches the orange the claude CLI uses
/// for its own prompt + spinner glyphs.  Plugin badges currently
/// hard-code this so the right-side decoration reads as "claudecode"
/// at a glance; if more plugins land we'll move colour into the wire
/// format alongside the text.
pub(crate) const PLUGIN_BADGE_FG: (f32, f32, f32) = (0.85, 0.47, 0.34);
/// Underline colour for auto-detected clickable spans (URLs, file
/// paths, email).  Calm cyan so it reads as "I'm a link" without
/// fighting ANSI-styled body text.
pub(crate) const LINK_UNDERLINE_FG: (f32, f32, f32) = (0.40, 0.70, 0.95);
// Selected-row BG kept as an alias of the cell-focused tone so
// sidebar selection and 9-grid focus read as the same affordance.
pub(crate) const STATE_ACTIVE: (f32, f32, f32) = (0.30, 0.85, 0.45);
pub(crate) const STATE_IDLE: (f32, f32, f32) = (0.55, 0.58, 0.62);
pub(crate) const STATE_EXITED: (f32, f32, f32) = (0.85, 0.30, 0.30);
