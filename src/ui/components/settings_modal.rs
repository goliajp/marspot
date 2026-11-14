//! Settings panel — where its rows go, and what the pointer is on.
//! The rows themselves are `settings_rows`.
//!
//! Same split as the other modals (`cc_usage_modal` is the reference):
//! shape and behaviour here, painting in `render_metal`.  The layout
//! is a list of cards, each holding rows separated by a hairline —
//! the shape the OS uses for exactly this job, and the shape a flat
//! stack of same-sized lines could never be spaced into.
//!
//! ## The rows are data
//!
//! A row is a label, a **cost**, and a control.  The cost line is not
//! decoration — it is the reason the row exists.  Every entry in this
//! panel is a decision marspot has chosen not to make for the user,
//! which means every entry has a downside worth naming; a settings
//! panel that lists only the upsides is asking the user to choose
//! blind.  Row text lives next to the row's effect for exactly that
//! reason: they cannot drift apart.
//!
//! Anything with a right answer is not here.  The rules that keep
//! reclamation safe — never the focused pane, never with work in
//! flight — are correctness, and a switch for them would only be a
//! way to break them.

use crate::settings::Settings;
pub use crate::ui::components::settings_choices::*;
pub use crate::ui::components::settings_rows::*;

/// What is this panel's own, in **typographic points**.  Everything
/// else -- header, paddings, card, control sizes -- is the shared
/// spec's (`ui::theme::spec`), so this panel cannot drift from the
/// others.
pub mod metric {
    use crate::ui::theme::{spec, PanelText};

    /// Panel width.  Sized for the longest cost line plus the widest
    /// control on the same row, with room to spare.
    pub const PANEL_W: f64 = 440.0;
    /// The title line's height -- that of the tallest control, so
    /// every row in a card is the same height whatever it holds.  A
    /// card whose rows jump between two heights reads as broken.
    pub const ROW_BAND_H: f64 = spec::control::SEGMENT_H;

    pub const LABEL: PanelText = PanelText::Label;
    pub const DESC: PanelText = PanelText::Secondary;
    pub const FOOTER: PanelText = PanelText::Caption;
}

/// A piece of the panel, with where it goes.  The walker emits these
/// in draw order; the painter draws them and the hit-test looks at
/// them, so a control cannot be clickable anywhere but where it is
/// drawn.
pub enum Slot {
    /// A group's heading: its line starts at `top`, at the card's
    /// left edge `x`, over the card's width `w`.
    Group { heading: &'static str, x: f64, top: f64, w: f64 },
    /// A group's card.  Emitted before the rows it contains.
    Card { rect: Rect },
    Row {
        row: Row,
        spec: &'static RowSpec,
        /// The row's full band inside the card — what a hover would
        /// highlight, and what the hairline sits at the bottom of.
        band: Rect,
        label_baseline: f64,
        desc_baseline: f64,
        /// Right-aligned in the card, centred on the title line.
        control: Rect,
    },
    /// Hairline between two rows of `card`, its top edge at `y`.
    Separator { card: Rect, y: f64 },
    Footer { baseline: f64 },
}

use marspot_term::layout::Rect;

use crate::ui::components::controls::{self, Measure};
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::theme::spec::{self, card, content, space};

/// One row's height — constant across a card whatever control it
/// holds, so the card does not step.
pub fn row_h_pt() -> f64 {
    card::ROW_PAD_Y * 2.0
        + metric::ROW_BAND_H
        + space::LABEL_TO_DESC
        + metric::DESC.cap()
        + metric::DESC.descent()
}

/// Walk the panel's content in draw order.  **One walker**, shared by
/// the painter, the hit-test and the height calculation — the
/// alternative is a layout pass plus a matching set of coordinates in
/// the click handler, which is how a control ends up reacting a row
/// above where it is drawn with nothing in a screenshot to say which
/// is wrong.
///
/// `rect` is the panel's frame in physical px.  The header is the
/// shared chrome's and is not walked; everything here lives in the
/// chrome's content rect.
///
/// `scroll` is how far the content has been pushed up under the
/// header.  It goes through the one walker the painter and the
/// hit-test share, so the two cannot disagree about it.
pub fn walk(rect: Rect, scroll: f64, s: &Settings, m: Measure<'_>, mut on: impl FnMut(Slot)) {
    let content = PanelChrome::new(rect, 0.0).content;
    let (card_x, card_w) = (content.x, content.w);
    let row_h = spec::px(row_h_pt());
    let band_h = spec::px(metric::ROW_BAND_H);

    let mut y = content.y_top - scroll;
    for (si, section) in SECTIONS.iter().enumerate() {
        if si > 0 {
            y += spec::px(space::CARD_TO_HEADING);
        }
        on(Slot::Group {
            heading: crate::ui::strings::t(section.heading),
            x: card_x,
            top: y,
            w: card_w,
        });
        y += panel_chrome::heading_height();

        let card = Rect { x: card_x, y_top: y, w: card_w, h: row_h * section.rows.len() as f64 };
        on(Slot::Card { rect: card });

        for (ri, row_spec) in section.rows.iter().enumerate() {
            let row_top = card.y_top + row_h * ri as f64;
            let band_top = row_top + spec::px(card::ROW_PAD_Y);
            let (ctl_w, ctl_h) = control_size(row_spec.row, s, m);
            on(Slot::Row {
                row: row_spec.row,
                spec: row_spec,
                band: Rect { x: card_x, y_top: row_top, w: card_w, h: row_h },
                // Label optically centred in the band, so a label and
                // the control beside it share one centre line.
                label_baseline: band_top + (band_h + spec::px(metric::LABEL.cap())) * 0.5,
                desc_baseline: band_top
                    + band_h
                    + spec::px(space::LABEL_TO_DESC + metric::DESC.cap()),
                control: Rect {
                    x: card_x + card_w - spec::px(card::ROW_PAD_X) - ctl_w,
                    y_top: band_top + (band_h - ctl_h) * 0.5,
                    w: ctl_w,
                    h: ctl_h,
                },
            });
            if ri + 1 < section.rows.len() {
                on(Slot::Separator { card, y: row_top + row_h });
            }
        }
        y = card.y_top + card.h;
    }

    y += spec::px(space::CARD_TO_FOOTER + metric::FOOTER.cap());
    on(Slot::Footer { baseline: y });
}

/// Everything under the header, in physical px, from the same walk
/// that draws it — so "the panel has a dead band at the bottom" is
/// not expressible.
pub fn content_h(s: &Settings, m: Measure<'_>) -> f64 {
    // The walk needs a rect; height does not depend on where it is.
    let probe = Rect { x: 0.0, y_top: 0.0, w: spec::px(metric::PANEL_W), h: 0.0 };
    let top = PanelChrome::new(probe, 0.0).content.y_top;
    let mut bottom = top;
    walk(probe, 0.0, s, m, |slot| {
        if let Slot::Footer { baseline } = slot {
            bottom = baseline;
        }
    });
    bottom - top
}

/// The panel's rect, centred, **sized by what it actually draws**.
pub fn panel_rect(
    w_phys: f64,
    h_phys: f64,
    s: &Settings,
    m: Measure<'_>,
    top_inset: f64,
) -> Rect {
    panel_chrome::frame_rect(
        w_phys,
        h_phys,
        spec::px(metric::PANEL_W),
        panel_chrome::frame_height(content_h(s, m)),
        top_inset,
    )
}

/// The same walk, with everything outside the content area dropped.
///
/// The paint path has no scissor -- `View::paint` says so in as many
/// words ("no automatic clipping") -- so a scrolled panel has to cull
/// its own slots or it draws over the header above it and the terminal
/// below. A card is clipped to the viewport rather than dropped, so
/// the background continues to the edge and the panel reads as having
/// more below; a row, a separator and a line of text are dropped
/// unless they fit whole, because half a row and half a glyph are what
/// the clip would have prevented.
pub fn walk_visible(
    rect: Rect,
    scroll: f64,
    s: &Settings,
    m: Measure<'_>,
    mut on: impl FnMut(Slot),
) {
    let content = PanelChrome::new(rect, 0.0).content;
    let top = content.y_top;
    let bottom = content.y_top + content.h;
    let inside = |y_top: f64, h: f64| y_top >= top - 1e-9 && y_top + h <= bottom + 1e-9;
    walk(rect, scroll, s, m, |slot| match slot {
        Slot::Card { rect: c } => {
            let y = c.y_top.max(top);
            let h = (c.y_top + c.h).min(bottom) - y;
            if h > 0.0 {
                on(Slot::Card { rect: Rect { y_top: y, h, ..c } });
            }
        }
        Slot::Separator { y, .. } if !inside(y, card::SEPARATOR_PX) => {}
        Slot::Row { band, .. } if !inside(band.y_top, band.h) => {}
        Slot::Group { top: t, .. } if !inside(t, spec::px(content::HEADING.cap())) => {}
        Slot::Footer { baseline }
            if !inside(baseline - spec::px(metric::FOOTER.cap()), spec::px(metric::FOOTER.cap())) => {}
        other => on(other),
    });
}

/// How far the content can be pushed up before its last pixel is at
/// the bottom of the content area.  Zero when it already fits.
pub fn max_scroll(rect: Rect, s: &Settings, m: Measure<'_>) -> f64 {
    (content_h(s, m) - PanelChrome::new(rect, 0.0).content.h).max(0.0)
}

/// `(width, height)` of this row's control, in physical px.
pub fn control_size(row: Row, s: &Settings, m: Measure<'_>) -> (f64, f64) {
    match row.control(s) {
        Control::Toggle(_) => controls::toggle_size(),
        Control::Segmented { options, .. } => controls::segmented_size(options, m),
    }
}

/// Left edge for a row's text.
pub fn text_x(rect: Rect) -> f64 {
    PanelChrome::new(rect, 0.0).content.x + spec::px(card::ROW_PAD_X)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strings::{t, Msg};
    use crate::ui::theme::spec as ui_spec;

    /// The panel's whole justification: every row names its downside.
    /// A row without one is a row that is asking the user to choose
    /// blind, and this catches it at compile-test time rather than in
    /// a screenshot.
    #[test]
    fn every_row_states_what_it_costs() {
        for r in rows() {
            assert!(!t(r.label).is_empty(), "{:?} has no label", r.row);
            assert!(
                t(r.cost).len() >= 12,
                "{:?} has no real cost line: {:?}",
                r.row,
                r.cost
            );
        }
        assert!(!SECTIONS.is_empty());
    }

    #[test]
    fn a_toggle_flips_and_a_segment_picks() {
        let s = Settings::default();
        let off = Row::ReclaimEnabled.apply(&s, 0).expect("flips");
        assert!(!off.reclaim_enabled);
        // Same click again comes back.
        assert!(Row::ReclaimEnabled.apply(&off, 0).unwrap().reclaim_enabled);

        let two_hours = Row::ReclaimIdleMinutes.apply(&s, 3).expect("picks");
        assert_eq!(two_hours.reclaim_idle_minutes, 120);
        // Picking what is already chosen writes nothing.
        assert!(Row::ReclaimIdleMinutes.apply(&s, 1).is_none());
        // "从不" is a value in the same control, not a second one.
        let never = Row::ReclaimIdleMinutes.apply(&s, 4).expect("never");
        assert_eq!(never.reclaim_idle_minutes, 0);
        assert_eq!(never.reclaim_after(), None);
    }

    /// With reclamation off, the rows describing how it behaves are
    /// inert — clicking one must not quietly change a value the user
    /// cannot see the effect of.
    #[test]
    fn the_rows_below_the_switch_go_inert_when_it_is_off() {
        let off = Settings { reclaim_enabled: false, ..Settings::default() };
        assert!(Row::ReclaimIdleMinutes.disabled_by(&off));
        assert!(Row::ReclaimPrefetch.disabled_by(&off));
        assert!(!Row::ReclaimEnabled.disabled_by(&off), "the switch itself stays live");
        assert!(Row::ReclaimIdleMinutes.apply(&off, 0).is_none());
        assert!(Row::ReclaimPrefetch.apply(&off, 0).is_none());
        assert!(Row::ReclaimEnabled.apply(&off, 0).is_some());
    }

    /// A value hand-edited to something the panel has no button for
    /// must survive being looked at.  Rounding it to the nearest
    /// offered choice would silently rewrite the user's file the
    /// moment they opened the panel.
    #[test]
    fn a_hand_edited_value_shows_as_nothing_chosen_and_is_not_rounded() {
        let s = Settings { reclaim_idle_minutes: 45, ..Settings::default() };
        match Row::ReclaimIdleMinutes.control(&s) {
            Control::Segmented { chosen, options } => {
                assert!(chosen >= options.len(), "45 must not light up a button");
            }
            _ => panic!("wrong control"),
        }
        // And merely rendering it changed nothing.
        assert_eq!(s.reclaim_idle_minutes, 45);
    }

    /// Stand-in for the font: SF Pro's average advance is close
    /// enough to 0.5 em for a geometry test, and the point of these
    /// assertions is that widths *come from a measurement* at all.
    fn fake_measure() -> impl FnMut(&str, f64, u16) -> f64 {
        |s: &str, pt: f64, w: u16| {
            let bold = if w >= 600 { 1.05 } else { 1.0 };
            s.chars().count() as f64 * pt * 0.5 * bold
                * crate::ui::core::ViewPainter::px_per_pt()
        }
    }

    /// `chrome_scale` is a process global that these tests both read
    /// (through `px_per_pt`) and write.  nextest gives each test its
    /// own process, which hides that; a plain `cargo test` runs them
    /// in one process and in parallel, where the density test's scale
    /// 2.0 lands in the middle of another test's layout and the rect
    /// and the slots inside it come out at different densities. The
    /// lock makes which runner you used stop mattering.
    static SCALE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ScaleGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for ScaleGuard {
        fn drop(&mut self) {
            crate::ui::set_chrome_scale(1.0);
        }
    }

    /// Hold for a test's whole body, not just while setting the scale.
    fn scale_guard() -> ScaleGuard {
        let g = SCALE.lock().unwrap_or_else(|e| e.into_inner());
        crate::ui::set_chrome_scale(1.0);
        ScaleGuard(g)
    }

    fn collect(rect: Rect, s: &Settings) -> Vec<Slot> {
        let mut m = fake_measure();
        let mut out = Vec::new();
        walk(rect, 0.0, s, &mut m, |slot| out.push(slot));
        out
    }

    fn rows_of(slots: &[Slot]) -> Vec<&Slot> {
        slots.iter().filter(|s| matches!(s, Slot::Row { .. })).collect()
    }

    fn test_rect(w: f64, h: f64, s: &Settings) -> Rect {
        let mut m = fake_measure();
        panel_rect(w, h, s, &mut m, 30.0)
    }

    /// Scrolled content goes under the header, and what is under the
    /// header is not drawn.
    #[test]
    fn a_control_scrolled_under_the_header_is_gone() {
        let _scale = scale_guard();
        let s = Settings::default();
        // A window too short for the panel, so there is somewhere to
        // scroll to.
        let rect = test_rect(1600.0, 700.0, &s);
        let mut m = fake_measure();
        let max = max_scroll(rect, &s, &mut m);
        assert!(max > 0.0, "the panel fits; this test needs it not to");
        let chrome = PanelChrome::new(rect, 0.0);
        let first = collect(rect, &s)
            .into_iter()
            .find_map(|slot| match slot {
                Slot::Row { control, .. } => Some(control),
                _ => None,
            })
            .expect("a row");
        // Push the first control up to the middle of the header.
        let scroll = first.y_top + first.h / 2.0 - (chrome.header.y_top + chrome.header.h / 2.0);
        assert!(scroll <= max, "the header is within reach of the scroll range");
        let mut drawn = 0;
        walk_visible(rect, scroll, &s, &mut m, |slot| {
            let (top, bottom) = match slot {
                Slot::Row { band, .. } => (band.y_top, band.y_top + band.h),
                Slot::Card { rect: c } => (c.y_top, c.y_top + c.h),
                _ => return,
            };
            drawn += 1;
            assert!(top >= chrome.content.y_top - 1e-9, "drawn over the header");
            assert!(bottom <= chrome.content.y_top + chrome.content.h + 1e-9, "drawn past the content");
        });
        assert!(drawn > 0, "something is still visible");
    }

    /// What the panel was rebuilt for, as assertions: rows must read
    /// as separate items inside a card, and nothing may overflow.
    #[test]
    fn rows_are_separated_and_nothing_overflows() {
        let _scale = scale_guard();
        let s = Settings::default();
        let rect = test_rect(1800.0, 1400.0, &s);
        let slots = collect(rect, &s);
        let mut m = fake_measure();
        let px = crate::ui::core::ViewPainter::px_per_pt();

        // Every row sits inside the card that was announced for it, and
        // every card inside the panel.
        let mut card: Option<Rect> = None;
        let mut rows_in_card = 0usize;
        for slot in &slots {
            match slot {
                Slot::Card { rect: c } => {
                    assert!(
                        c.x >= rect.x && c.x + c.w <= rect.x + rect.w + 1e-9,
                        "card escapes the panel"
                    );
                    card = Some(*c);
                    rows_in_card = 0;
                }
                Slot::Row { row, band, label_baseline, desc_baseline, control, .. } => {
                    let c = card.expect("a row before any card");
                    rows_in_card += 1;
                    assert!(
                        band.y_top >= c.y_top - 1e-9
                            && band.y_top + band.h <= c.y_top + c.h + 1e-9,
                        "{row:?} band escapes its card"
                    );
                    // Two-tier text: the cost line is under the label,
                    // and it is *smaller*, not merely dimmer.
                    assert!(desc_baseline > label_baseline, "{row:?} cost line above its label");
                    assert!(metric::DESC.pt() < metric::LABEL.pt(), "no type scale");
                    // The control clears the cost line under it — the
                    // reason the label line is a band, not a baseline.
                    assert!(
                        control.y_top + control.h
                            <= desc_baseline
                                - metric::DESC.cap()
                                    * px
                                + 1e-9,
                        "{row:?} control overlaps its own cost line"
                    );
                    // Right-aligned on one column, inside the card.
                    let want = c.x + c.w - ui_spec::card::ROW_PAD_X * px;
                    assert!(
                        (control.x + control.w - want).abs() < 0.5,
                        "{row:?} control off the column"
                    );
                    if let Control::Segmented { options, .. } = row.control(&s) {
                        for (n, label) in options.iter().enumerate() {
                            let sr = controls::segment_rect(*control, n, options, &mut m);
                            let ink = fake_measure()(t(*label), ui_spec::control::SEGMENT_TEXT.pt(), ui_spec::control::SEGMENT_TEXT.weight());
                            assert!(sr.w > ink, "{label:?} needs {ink:.1}, button is {:.1}", sr.w);
                        }
                        let last =
                            controls::segment_rect(*control, options.len() - 1, options, &mut m);
                        assert!(
                            last.x + last.w <= control.x + control.w + 0.5,
                            "the last segment runs past the control"
                        );
                    }
                }
                Slot::Separator { card: of, y } => {
                    let c = card.expect("a separator before any card");
                    let sep = panel_chrome::row_separator_rect(*of, *y);
                    assert!(
                        sep.x > c.x && sep.x + sep.w < c.x + c.w,
                        "the hairline must be inset from both of the card's edges"
                    );
                    assert!(rows_in_card >= 1, "a hairline before the first row");
                }
                _ => {}
            }
        }

        // A single-row card gets no hairline; a three-row card gets two.
        let seps = slots.iter().filter(|s| matches!(s, Slot::Separator { .. })).count();
        let expect: usize = SECTIONS.iter().map(|s| s.rows.len() - 1).sum();
        assert_eq!(seps, expect, "one hairline between each pair of rows, none at the edges");

        // No dead band: the footer sits just above the bottom edge.
        let foot = slots
            .iter()
            .find_map(|s| match s {
                Slot::Footer { baseline } => Some(*baseline),
                _ => None,
            })
            .expect("footer");
        let bottom = rect.y_top + rect.h;
        assert!(foot < bottom, "footer outside the panel");
        assert!(
            bottom - foot < (ui_spec::content::PAD_BOTTOM + metric::FOOTER.pt()) * px,
            "dead space under the footer: {:.1}px",
            bottom - foot
        );
    }

    /// The panel is roomy on purpose — one of the two complaints that
    /// started this rewrite was that everything was crammed together.
    ///
    /// Stated **relative to the type**, not in absolute points: the
    /// first cut asserted "a row is at least 44pt", which fired the
    /// moment the type came down a rung even though the proportions
    /// were unchanged.  A test that has to be edited whenever the
    /// scale moves is not guarding the thing it claims to guard.
    #[test]
    fn a_row_is_mostly_space_not_text() {
        let _scale = scale_guard();
        let s = Settings::default();
        let rect = test_rect(2400.0, 1800.0, &s);
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let slots = collect(rect, &s);
        let rows: Vec<_> = rows_of(&slots)
            .iter()
            .map(|s| match s {
                Slot::Row { band, .. } => *band,
                _ => unreachable!(),
            })
            .collect();
        for w in rows.windows(2) {
            let gap = w[1].y_top - (w[0].y_top + w[0].h);
            assert!(gap >= -1e-9, "rows overlap");
        }
        // Ink versus room: the two lines of a row must not fill it.
        let ink = metric::LABEL.cap() + ui_spec::space::LABEL_TO_DESC + metric::DESC.cap();
        let row_h = rows[0].h / px;
        assert!(
            ink / row_h < 0.62,
            "a row is {:.0}% text — that is the cramped panel again",
            ink / row_h * 100.0,
        );
        // And the text column holds a real sentence at the label size,
        // whatever that size currently is.
        let column = (rect.w / px) - 2.0 * (ui_spec::content::PAD_X + ui_spec::card::ROW_PAD_X);
        assert!(
            column > 30.0 * metric::LABEL.cap(),
            "text column {column:.0}pt is narrow for {:.1}pt type",
            metric::LABEL.pt(),
        );
    }

    /// The same geometry, measured with the **real font**.
    ///
    /// `fake_measure` proves the layout is self-consistent; it cannot
    /// prove SF Pro fits, and "the button is narrower than the label
    /// inside it" is precisely a real-metrics failure.  Skipped rather
    /// than failed if the font stack will not build.
    #[test]
    fn the_real_font_fits_the_boxes_drawn_for_it() {
        real_font_fits();
    }

    /// The same in Japanese and in Chinese: a description a translator
    /// wrote one clause too long runs out of its card, and only real
    /// metrics of the real font can say so.
    #[test]
    fn the_real_font_fits_the_boxes_drawn_for_it_in_japanese() {
        crate::snapshot_kit::in_language(marspot_term::language::LangId::Ja, real_font_fits);
    }

    #[test]
    fn the_real_font_fits_the_boxes_drawn_for_it_in_chinese() {
        crate::snapshot_kit::in_language(marspot_term::language::LangId::ZhHans, real_font_fits);
    }

    fn real_font_fits() {
        let _scale = scale_guard();
        let Ok(mut font) = crate::font_cache::FontCache::build() else {
            eprintln!("no font stack; skipping");
            return;
        };
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let s = Settings::default();
        let mut m = |t: &str, pt: f64, w: u16| {
            font.measure_ui_text_at_size(
                t, w, crate::font_shape::ShapeOptions::default(), pt,
            )
        };
        let rect = panel_rect(2000.0, 1500.0, &s, &mut m, 30.0);

        let mut slots = Vec::new();
        walk(rect, 0.0, &s, &mut m, |slot| slots.push(slot));

        let mut card = Rect { x: 0.0, y_top: 0.0, w: 0.0, h: 0.0 };
        for slot in &slots {
            match slot {
                Slot::Card { rect: c } => card = *c,
                Slot::Row { row, spec, control, .. } => {
                    let text_left = card.x + ui_spec::card::ROW_PAD_X * px;
                    // The cost line is the long text in this panel: it
                    // must fit the card, or it runs out of the box.
                    let room = card.w - 2.0 * ui_spec::card::ROW_PAD_X * px;
                    // the language row has a second line it can show
                    let costs = [Some(spec.cost), (*row == Row::Language).then_some(Msg::CostLanguageRelaunch)];
                    for cost in costs.into_iter().flatten() {
                        let cost_w = m(t(cost), metric::DESC.pt(), metric::DESC.weight());
                        assert!(
                            cost_w <= room,
                            "{row:?}: {:?} needs {:.0}pt and the card gives {:.0}pt",
                            t(cost),
                            cost_w / px,
                            room / px,
                        );
                    }
                    // the check can fail: a line and a half does not fit
                    let long = format!("{0}{0}", t(Msg::CostClaudeCodeReportsModel));
                    assert!(m(&long, metric::DESC.pt(), metric::DESC.weight()) > room, "{long:?} fits");
                    // The label and the control share one line and must
                    // not collide.
                    let label_w = m(t(spec.label), metric::LABEL.pt(), metric::LABEL.weight());
                    assert!(
                        text_left + label_w + 12.0 * px <= control.x,
                        "{:?}: label runs into its control",
                        row
                    );
                    if let Control::Segmented { options, .. } = row.control(&s) {
                        for (n, label) in options.iter().enumerate() {
                            let seg = controls::segment_rect(*control, n, options, &mut m);
                            let ink = m(t(*label), ui_spec::control::SEGMENT_TEXT.pt(), ui_spec::control::SEGMENT_TEXT.weight());
                            assert!(
                                seg.w >= ink + 2.0 * ui_spec::control::SEGMENT_PAD_X * px - 0.5,
                                "{label:?} ink {:.0}pt vs button {:.0}pt",
                                ink / px,
                                seg.w / px,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        // And the panel it all sits in is the size it claims.
        assert!(rect.w > 0.0 && rect.h > 0.0);
    }

    /// Every geometric promise this panel makes, at **both** densities.
    ///
    /// The panels were only ever laid out and eyeballed on a display
    /// whose backing scale is 1; every scale bug this session had —
    /// the toolbar on the traffic lights, the cards outside their
    /// modal, boxes doubling while their text stood still — was a
    /// second density nobody had rendered.  So the invariants run at
    /// both, under the scale lock.
    #[test]
    fn the_panel_holds_together_at_every_density() {
        let _scale = scale_guard();
        for scale in [1.0f64, 2.0] {
            crate::ui::set_chrome_scale(scale);
            let px = crate::ui::core::ViewPainter::px_per_pt();
            let s = Settings::default();
            let rect = test_rect(2400.0 * scale, 1800.0 * scale, &s);
            let slots = collect(rect, &s);
            let mut m = fake_measure();

            // The panel scales with the density rather than staying a
            // fixed pixel count — a panel that did not would be half
            // its intended size on a retina Mac.
            assert!(
                (rect.w - metric::PANEL_W * px).abs() < 1.0,
                "scale {scale}: panel {:.0}px is not {:.0}pt",
                rect.w, metric::PANEL_W,
            );

            let mut card: Option<Rect> = None;
            let mut rows = 0usize;
            for slot in &slots {
                match slot {
                    Slot::Card { rect: c } => {
                        assert!(
                            c.x >= rect.x - 1e-6
                                && c.x + c.w <= rect.x + rect.w + 1e-6,
                            "scale {scale}: card escapes the panel",
                        );
                        card = Some(*c);
                    }
                    Slot::Row { row, band, control, desc_baseline, .. } => {
                        let c = card.expect("row before card");
                        rows += 1;
                        assert!(
                            band.y_top >= c.y_top - 1e-6
                                && band.y_top + band.h <= c.y_top + c.h + 1e-6,
                            "scale {scale}: {row:?} band escapes its card",
                        );
                        assert!(
                            control.x + control.w <= c.x + c.w + 1e-6,
                            "scale {scale}: {row:?} control escapes its card",
                        );
                        assert!(
                            control.y_top + control.h <= *desc_baseline + 1e-6,
                            "scale {scale}: {row:?} control overlaps its cost line",
                        );
                        if let Control::Segmented { options, .. } = row.control(&s) {
                            let last = controls::segment_rect(
                                *control, options.len() - 1, options, &mut m,
                            );
                            assert!(
                                last.x + last.w <= control.x + control.w + 0.5,
                                "scale {scale}: the last segment runs past the control",
                            );
                        }
                    }
                    Slot::Footer { baseline } => assert!(
                        *baseline < rect.y_top + rect.h,
                        "scale {scale}: the footer fell out of the panel",
                    ),
                    _ => {}
                }
            }
            assert_eq!(rows, super::rows().count(), "scale {scale}: rows lost");
        }
    }

    #[test]
    fn the_panel_fits_the_window_it_is_centred_in() {
        let _scale = scale_guard();
        let s = Settings::default();
        for (w, h) in [(1200.0, 800.0), (400.0, 300.0), (3840.0, 2160.0)] {
            let r = panel_rect(w, h, &s, &mut fake_measure(), 30.0);
            assert!(r.w <= w, "wider than the window at {w}x{h}");
            assert!(r.h <= h, "taller than the window at {w}x{h}");
            assert!(r.x >= 0.0 && r.y_top >= 0.0, "off-screen at {w}x{h}");
        }
    }
}
