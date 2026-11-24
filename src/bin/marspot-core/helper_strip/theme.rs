//! The colours of Claude Code's own list of subagents, theme by theme.

use marspot_term::grid::{CellAttrs, Color, Grid};

#[derive(Debug, PartialEq)]
pub(in super::super) struct Theme {
    /// What is not in use: the colour, and whether it is faint as well.
    dim: (Color, bool),
    selected: Color,
    success: Color,
    error: Color,
}

const fn rgb(dim: u8, selected: [u8; 3], success: [u8; 3], error: [u8; 3]) -> Theme {
    const fn of(c: [u8; 3]) -> Color {
        Color::rgb(c[0], c[1], c[2])
    }
    Theme { dim: (of([dim; 3]), false), selected: of(selected), success: of(success), error: of(error) }
}

const fn indexed(dim: (u8, bool), selected: u8, success: u8, error: u8) -> Theme {
    Theme {
        dim: (Color::indexed(dim.0), dim.1),
        selected: Color::indexed(selected),
        success: Color::indexed(success),
        error: Color::indexed(error),
    }
}

/// Dark, light, the two for colour blindness, the two in the sixteen
/// ANSI colours, and dark in 256 colours.
pub(super) static THEMES: [Theme; 7] = [
    rgb(153, [177, 185, 249], [78, 186, 101], [255, 107, 128]),
    rgb(102, [87, 105, 247], [44, 122, 57], [171, 43, 63]),
    rgb(153, [153, 204, 255], [51, 153, 255], [255, 102, 102]),
    rgb(102, [51, 102, 255], [0, 102, 153], [204, 0, 0]),
    indexed((7, true), 12, 10, 9),
    indexed((8, false), 4, 2, 1),
    indexed((246, false), 153, 114, 211),
];

impl Theme {
    /// The theme for a terminal's own paper.
    pub(in super::super) fn for_paper(light: bool) -> &'static Theme {
        &THEMES[light as usize]
    }

    /// The theme the agent on `screen` draws in, by what is under its
    /// input box.  Two themes can share a dim colour; one whose other
    /// colours are there too is the likelier.
    pub(in super::super) fn of_screen(screen: &Grid) -> Option<&'static Theme> {
        let rule = (0..screen.rows()).rev().find(|&r| screen.cell(0, r).ch == '─')?;
        let (mut dim, mut more) = ([false; THEMES.len()], [false; THEMES.len()]);
        for row in rule + 1..screen.rows() {
            for col in 0..screen.cols() {
                let cell = screen.cell(col, row);
                if matches!(cell.ch, ' ' | '\0') {
                    continue;
                }
                for (i, t) in THEMES.iter().enumerate() {
                    dim[i] |= (cell.attrs.fg, cell.attrs.dim) == t.dim;
                    more[i] |= [t.selected, t.success, t.error].contains(&cell.attrs.fg);
                }
            }
        }
        let best = (0..THEMES.len()).filter(|&i| dim[i]).min_by_key(|&i| !more[i])?;
        Some(&THEMES[best])
    }

    pub(super) fn plain(bold: bool) -> CellAttrs {
        CellAttrs { bold, ..CellAttrs::default() }
    }

    pub(super) fn dim(&self) -> CellAttrs {
        CellAttrs { fg: self.dim.0, dim: self.dim.1, ..CellAttrs::default() }
    }

    pub(super) fn selected(&self, bold: bool) -> CellAttrs {
        CellAttrs { fg: self.selected, ..Self::plain(bold) }
    }

    pub(super) fn success(&self, bold: bool) -> CellAttrs {
        CellAttrs { fg: self.success, ..Self::plain(bold) }
    }

    pub(super) fn error(&self, bold: bool) -> CellAttrs {
        CellAttrs { fg: self.error, ..Self::plain(bold) }
    }
}
