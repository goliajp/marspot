//! Finding an agent's input box on a screen, and what is typed in it.

use super::row_text_of;

/// Claude Code draws U+276F, Codex U+203A.
const PROMPTS: [char; 2] = ['\u{276f}', '\u{203a}'];

/// Where Kimi Code's prompt is on `row`, if the row is the first of
/// its box.  The prompt is a plain `>`, which a shell or a quoted line
/// draws too, so it counts only as the first thing inside a box's left
/// side: `│ > `.
fn kimi_prompt_col(grid: &marspot_term::grid::Grid, row: u16) -> Option<u16> {
    let mut drawn = (0..grid.cols()).filter(|&c| !matches!(grid.cell(c, row).ch, ' ' | '\0'));
    let (side, glyph) = (drawn.next()?, drawn.next()?);
    (grid.cell(side, row).ch == '│' && grid.cell(glyph, row).ch == '>' && glyph == side + 2).then_some(glyph)
}
/// The box's own sides, when it is drawn as a box.
const SIDES: [char; 3] = ['│', '┃', '|'];

fn is_rule(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| matches!(c, '─' | '━' | '╭' | '╮' | '╰' | '╯'))
}

/// One cell of the box as typed: its column, its width, its text.
type Glyph = (u16, u16, String);

/// Not knowing reads as nothing throughout: pasting a guess into
/// someone's input is worse than losing a line they can retype.
pub(super) fn composer_of(grid: &marspot_term::grid::Grid) -> Option<String> {
    box_of(grid).filter(|t| !t.is_empty())
}

/// Where an agent's input box is: the row of its prompt, and the row
/// under its last, where its rule or its shade ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxAt {
    pub prompt_row: u16,
    pub end: u16,
    glyph_col: u16,
    codex: bool,
}

fn bare(grid: &marspot_term::grid::Grid, r: u16) -> String {
    let line = row_text_of(grid, r, 0, |c| grid.cell(c, r));
    line.trim().trim_start_matches(SIDES).trim_start().to_string()
}

/// The input box the cursor is in, if it is in one.
pub fn box_at(grid: &marspot_term::grid::Grid) -> Option<BoxAt> {
    use marspot_term::grid::Color;
    let bg = |r: u16| grid.cell(0, r).attrs.bg;
    let bare = |r: u16| bare(grid, r);
    // Up from the cursor to the prompt it is typing after.  A rule or a
    // change of shade on the way means the cursor is not in the box.
    let (_, cursor_row) = grid.cursor();
    let mut row = cursor_row;
    while !bare(row).starts_with(PROMPTS) && kimi_prompt_col(grid, row).is_none() {
        if row == 0 || is_rule(&bare(row)) || bg(row) != bg(cursor_row) {
            return None;
        }
        row -= 1;
    }
    let kimi = kimi_prompt_col(grid, row);
    let glyph_col = kimi.or_else(|| (0..grid.cols()).find(|&c| PROMPTS.contains(&grid.cell(c, row).ch)))?;
    let codex = kimi.is_none() && grid.cell(glyph_col, row).ch == '\u{203a}';
    // Claude Code and Kimi rule the box off above; Codex shades it.  A
    // prompt glyph without either is a message drawn in the transcript.
    let boxed = if codex { bg(row) != Color::DEFAULT } else { row > 0 && is_rule(&bare(row - 1)) };
    if !boxed {
        return None;
    }
    // Down to the box's end: the rule under it, or where the shade
    // stops.  Blank rows before that are the person's own blank lines.
    let end = (row + 1..grid.rows()).find(|&r| is_rule(&bare(r)) || bg(r) != bg(row))?;
    Some(BoxAt { prompt_row: row, end, glyph_col, codex })
}

/// What the input box holds, empty for an empty one; None when the
/// cursor is not in a box.
pub fn box_of(grid: &marspot_term::grid::Grid) -> Option<String> {
    let BoxAt { prompt_row: row, end, glyph_col, codex } = box_at(grid)?;
    let lines: Vec<Vec<Glyph>> = (row..end).map(|r| typed_glyphs(grid, r, glyph_col + 2)).collect();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str(joint(&lines[i - 1], line, grid.cols(), codex));
        }
        line.iter().for_each(|g| out.push_str(&g.2));
    }
    Some(out.trim().to_string())
}

/// A row of the box from `from` on, dim cells blanked, without the
/// blanks and box side that pad it out to the right.
fn typed_glyphs(grid: &marspot_term::grid::Grid, row: u16, from: u16) -> Vec<Glyph> {
    let mut clusters = Vec::new();
    grid.row_clusters_at_view(0, row, &mut clusters);
    let mut out: Vec<Glyph> = Vec::new();
    for c in from..grid.cols() {
        let cell = grid.cell(c, row);
        if cell.ch == '\0' {
            continue;
        }
        let wide = c + 1 < grid.cols() && grid.cell(c + 1, row).ch == '\0';
        let text = if cell.attrs.dim {
            " ".to_string()
        } else {
            match grid
                .cluster_text(&cell)
                .or_else(|| marspot_term::grid::cluster_in_row(&clusters, c))
            {
                Some(t) => t.to_string(),
                None => cell.ch.to_string(),
            }
        };
        out.push((c, if wide { 2 } else { 1 }, text));
    }
    let trailing_blank = |o: &mut Vec<Glyph>| {
        while o.last().is_some_and(|g| g.2.trim().is_empty()) {
            o.pop();
        }
    };
    trailing_blank(&mut out);
    if out.last().is_some_and(|g| g.2.chars().all(|ch| SIDES.contains(&ch))) {
        out.pop();
        trailing_blank(&mut out);
    }
    out
}

/// What stood between two rows of the box before it was drawn.
///
/// The box wraps a long line by itself, so a row break is either the
/// person's newline or the program's wrap, and the screen does not say
/// which.  The wrap does leave a mark: it only moves text down that
/// would not have fit.  So if the next row's first word fits after the
/// previous row, the break was the person's.  Words are what each
/// program wraps by -- Claude Code only at spaces, so a run of Chinese
/// is one word; Codex between any two wide characters too.  A wrap at
/// a space ate the space; between two wide characters there was none.
fn joint(prev: &[Glyph], next: &[Glyph], cols: u16, codex: bool) -> &'static str {
    let (Some(last), Some(first)) = (prev.last(), next.first()) else {
        return "\n";
    };
    // A wrap never starts a row with a space: the space is what it
    // broke at.  Leading blanks are the person's indentation.
    if first.2 == " " {
        return "\n";
    }
    let both_wide = last.1 == 2 && first.1 == 2;
    let word: u16 = if codex && first.1 == 2 {
        2
    } else {
        next.iter()
            .take_while(|g| g.2 != " " && !(codex && g.1 == 2))
            .map(|g| g.1)
            .sum()
    };
    let gap = if codex && both_wide { 0 } else { 1 };
    // Codex never writes the last column: over the panes measured its
    // rows stop at 72 of 73, Claude Code's reach 73.
    let width = if codex { cols - 1 } else { cols };
    if last.0 + last.1 + gap + word <= width {
        "\n"
    } else if both_wide {
        ""
    } else {
        " "
    }
}
