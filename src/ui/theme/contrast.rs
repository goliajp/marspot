//! WCAG contrast between two colours, and the test that holds every
//! theme's table to it.

use crate::ui::core::Color;

/// WCAG 2 relative luminance of an opaque sRGB colour.
pub fn luminance(c: Color) -> f64 {
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG 2 contrast ratio, 1.0 (the same colour) to 21.0 (black on
/// white).
pub fn ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `top` at opacity `alpha` over an opaque `bottom`: what a tinted fill
/// looks like once it is on its surface.
pub fn over(top: Color, alpha: f64, bottom: Color) -> Color {
    let mix = |t: u8, b: u8| (t as f64 * alpha + b as f64 * (1.0 - alpha)).round() as u8;
    Color::rgb(mix(top.r, bottom.r), mix(top.g, bottom.g), mix(top.b, bottom.b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::spec;
    use crate::ui::theme::table::{Theme, CONTRAST, DARK, LIGHT};
    use marspot_term::palette::ThemeId;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Kind {
        /// Text that has to be read.
        Text,
        /// Text that recedes on purpose.
        Secondary,
        /// Not text: a cursor, a ring, a dot, a knob, a cross.
        Mark,
        /// One of the terminal's sixteen on the terminal's ground.
        Ansi,
        /// The label of a control that cannot be used.  WCAG exempts
        /// it; it is listed so the number is seen.
        Disabled,
    }

    /// The lowest ratio a pair of `kind` may have under theme `id`.
    fn floor(id: ThemeId, kind: Kind) -> f64 {
        match (id, kind) {
            (_, Kind::Disabled) => 1.0,
            (ThemeId::Contrast, Kind::Text | Kind::Secondary) => 7.0,
            (ThemeId::Contrast, Kind::Mark | Kind::Ansi) => 4.5,
            (_, Kind::Text) => 4.5,
            (_, Kind::Secondary | Kind::Mark | Kind::Ansi) => 3.0,
        }
    }

    const ANSI: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "bright black",
        "bright red", "bright green", "bright yellow", "bright blue", "bright magenta",
        "bright cyan", "bright white",
    ];

    /// The ANSI colours that by convention are the ground itself:
    /// black on a dark terminal, the two whites on a light one.
    /// (Bright black is not among them: it is how programs write
    /// comments and has to be readable.)
    fn ansi_is_the_ground(id: ThemeId, index: usize) -> bool {
        match id {
            ThemeId::Dark | ThemeId::Contrast => index == 0,
            ThemeId::Light => index == 7 || index == 15,
        }
    }

    /// Every (what is drawn, what it is drawn on) the interface has,
    /// by the roles `panel_palette` hands out.
    fn pairs(t: &Theme) -> Vec<(Kind, String, Color, Color)> {
        let unit = |c: (f64, f64, f64)| Color::from_rgba_f32([c.0 as f32, c.1 as f32, c.2 as f32, 1.0]);
        let (frame, card, raised) = (t.bg_panel, t.surface_3, t.surface_4);
        let (chrome, canvas) = (t.chrome, t.canvas);
        let accent = t.bg_selected;
        let selected = spec::table::SELECTED_ALPHA as f64;
        let chip = spec::control::CHIP_FILL_ALPHA as f64;
        let rest = spec::control::DANGER_REST_ALPHA as f64;
        let (term_fg, term_bg) = (unit(t.term.fg), unit(t.term.bg));
        use Kind::*;
        let mut out: Vec<(Kind, String, Color, Color)> = [
            (Text, "text on a panel", t.fg, frame),
            (Text, "text on a card", t.fg, card),
            (Text, "text on a row under the pointer", t.fg, raised),
            (Text, "text on a selected row of a card", t.fg, over(accent, selected, card)),
            (Text, "text on a selected row of a panel, a menu, the search list", t.fg, over(accent, selected, frame)),
            (Text, "text on the sidebar's selected row", t.fg, over(accent, selected, chrome)),
            (Text, "text on a sidebar row under the pointer", t.fg, t.bg_hover),
            (Text, "title of the pane in use", t.fg, canvas),
            (Text, "text on the window", t.fg, chrome),
            (Text, "label on the accent (primary button, picked segment)", t.fg_on_accent, accent),
            (Text, "accent as text on a card", t.accent, card),
            (Text, "accent as text on a panel", t.accent, frame),
            (Text, "terminal text on the terminal's background", term_fg, term_bg),
            (Text, "terminal text in the pane in use", term_fg, canvas),
            (Text, "terminal text in a pane not in use", term_fg, chrome),
            (Text, "terminal text on a selection", term_fg, t.selection),
            (Text, "text on a search hit", t.search_hit_fg, t.search_hit),
            (Text, "input method text", t.ime_fg, t.ime_bg),
            (Text, "link in the pane in use", t.link, canvas),
            (Text, "link in a pane not in use", t.link, chrome),
            (Text, "banner text", t.notice_fg, over(t.notice_bg, t.notice_bg.a, term_bg)),
            (Secondary, "secondary text on a panel", t.fg_secondary, frame),
            (Secondary, "secondary text on a card", t.fg_secondary, card),
            (Secondary, "secondary text on a segment, a key cap", t.fg_secondary, raised),
            (Secondary, "pane name on the window (sidebar, title strip)", t.fg_secondary, chrome),
            (Secondary, "muted text on a panel", t.fg_muted, frame),
            (Secondary, "muted text on a card", t.fg_muted, card),
            (Secondary, "muted text and icons on the window", t.fg_muted, chrome),
            (Text, "ok chip", t.success_text, over(t.success, chip, card)),
            (Text, "warn chip", t.warn_text, over(t.warn, chip, card)),
            (Text, "danger chip", t.danger_text, over(t.danger, chip, card)),
            (Text, "ok percentage on a card", t.success_text, card),
            (Text, "warn percentage on a card", t.warn_text, card),
            (Text, "danger percentage on a card", t.danger_text, card),
            (Text, "agent badge in the pane in use", t.agent, canvas),
            (Text, "agent badge on the window", t.agent, chrome),
            (Mark, "cursor on the terminal's background", t.cursor_text, term_bg),
            (Mark, "focus ring on the window", t.focus_ring, chrome),
            (Mark, "knob on a toggle that is on", t.knob, accent),
            (Mark, "knob on a toggle that is off", t.knob, t.toggle_off),
            (Mark, "accent bar on a card", t.accent, card),
            (Mark, "ok dot on the window", t.success, chrome),
            (Mark, "danger dot on the window", t.danger, chrome),
            (Mark, "danger mark on its resting fill", t.danger_text, over(t.danger, rest, card)),
            (Mark, "mark on the danger fill", t.fg_on_accent, t.danger),
            (Disabled, "disabled label on a card", t.fg_disabled, card),
            (Disabled, "disabled label on a segment", t.fg_disabled, raised),
        ]
        .into_iter()
        .map(|(kind, what, fg, on)| (kind, what.to_string(), fg, on))
        .collect();
        for (i, name) in ANSI.iter().enumerate() {
            if ansi_is_the_ground(t.id, i) {
                continue;
            }
            let c = unit(t.term.ansi[i]);
            out.push((Ansi, format!("ansi {name} in the pane in use"), c, canvas));
            out.push((Ansi, format!("ansi {name} in a pane not in use"), c, chrome));
        }
        out
    }

    /// The pairs of `t` that fall below their floor.
    fn failures(t: &Theme) -> Vec<String> {
        pairs(t)
            .into_iter()
            .filter(|(kind, _, fg, on)| ratio(*fg, *on) < floor(t.id, *kind))
            .map(|(_, what, ..)| what)
            .collect()
    }

    #[test]
    fn contrast_is_the_wcag_ratio() {
        assert!((ratio(Color::BLACK, Color::WHITE) - 21.0).abs() < 1e-9);
        assert_eq!(ratio(Color::rgb(90, 90, 90), Color::rgb(90, 90, 90)), 1.0);
        // #767676 on white is the textbook 4.5:1 grey.
        let grey = ratio(Color::rgb(0x76, 0x76, 0x76), Color::WHITE);
        assert!((grey - 4.54).abs() < 0.01, "{grey}");
        assert_eq!(over(Color::WHITE, 0.5, Color::BLACK), Color::rgb(128, 128, 128));
    }

    #[test]
    fn contrast_holds_in_every_theme() {
        let themes = [&DARK, &LIGHT, &CONTRAST];
        let tables: Vec<_> = themes.iter().map(|t| pairs(t)).collect();
        println!("{:<60} {:>7} {:>7} {:>9}", "pair", "dark", "light", "contrast");
        for (_, what, ..) in &tables[0] {
            let cell = |i: usize| {
                tables[i]
                    .iter()
                    .find(|(_, w, ..)| w == what)
                    .map_or("—".to_string(), |(_, _, fg, on)| format!("{:.2}", ratio(*fg, *on)))
            };
            println!("{what:<60} {:>7} {:>7} {:>9}", cell(0), cell(1), cell(2));
        }
        // The light theme exempts other ANSI colours than the dark
        // ones do, so its rows are not all in the first table.
        for (_, what, fg, on) in tables[1].iter().filter(|(_, w, ..)| !tables[0].iter().any(|(_, d, ..)| d == w)) {
            println!("{what:<60} {:>7} {:>7.2} {:>9}", "—", ratio(*fg, *on), "—");
        }
        assert_eq!(failures(&DARK), Vec::<String>::new(), "dark");
        assert_eq!(failures(&LIGHT), Vec::<String>::new(), "light");
        assert_eq!(failures(&CONTRAST), Vec::<String>::new(), "high contrast");
    }

    /// The walk has to be able to fail: one bad colour in an otherwise
    /// good table is found, in each kind of pair.
    #[test]
    fn contrast_walk_finds_a_bad_pair() {
        let grey_text = Theme { fg_secondary: Color::rgb(200, 204, 210), ..copy(&LIGHT) };
        assert!(failures(&grey_text).contains(&"secondary text on a card".to_string()));
        let white_label = Theme { fg_on_accent: Color::WHITE, ..copy(&CONTRAST) };
        assert_eq!(failures(&white_label), ["label on the accent (primary button, picked segment)", "mark on the danger fill"]);
        let pale_cursor = Theme { cursor_text: Color::rgb(240, 240, 240), ..copy(&LIGHT) };
        assert_eq!(failures(&pale_cursor), ["cursor on the terminal's background"]);
        let paper = &marspot_term::palette::LIGHT;
        let mut ansi = paper.ansi;
        ansi[3] = (1.0, 0.9, 0.2);
        let yellow_on_white: &'static _ = Box::leak(Box::new(marspot_term::palette::Palette {
            id: paper.id,
            bg: paper.bg,
            fg: paper.fg,
            cursor: paper.cursor,
            ansi,
        }));
        let bad_ansi = Theme { term: yellow_on_white, ..copy(&LIGHT) };
        assert_eq!(
            failures(&bad_ansi),
            ["ansi yellow in the pane in use", "ansi yellow in a pane not in use"],
        );
    }

    fn copy(t: &Theme) -> Theme {
        Theme { ..*t }
    }
}
