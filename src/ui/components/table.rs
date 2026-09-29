//! `Table` — devops-grade tabular display component.  Used wherever
//! marspot needs a real grid of (column, row) cells: the Process
//! Monitor master pane list + detail process tree, future stats /
//! settings panes, anything where alignment + sortable headers
//! + indented hierarchies matter.
//!
//! Design notes:
//!
//! - **No scroll, no state.**  Caller owns selection / hovered_row /
//!   scroll_y / sort key.  Table is a pure renderer over these snap-
//!   shots.  Scroll affects the BODY only — header stays sticky.
//! - **Column widths** mix fixed (`Px`) and proportional (`Flex`).
//!   Layout solves for column rects given the table's `rect.w` minus
//!   the sum of fixed widths; remainder splits proportionally.
//! - **Alignment** is per column, applied via
//!   `ViewPainter::text_in(rect, …, align)` so character-perfect
//!   right-alignment of numeric columns "just works".
//! - **Tree-style indentation** is supported via `TableRow.depth` —
//!   shifts the FIRST column's text only, leaves the others square.
//! - **Sort indicator** ▲/▼ glyph painted in the column header
//!   when `TableColumn.sort` is `Some(…)`.  Clickable headers light
//!   up only when both `sortable: true` and `hovered_header == i`.
//!
//! Hit testing is exposed via `hit_test_row` / `hit_test_header` so
//! `mouse_down` can dispatch without re-doing layout math.

use marspot_term::layout::{Rect, Alignment};
use crate::ui::core::ViewPainter;

/// Strategy for sizing one column.
#[derive(Debug, Clone, Copy)]
pub enum ColumnWidth {
    /// Fixed pixels.  Used for numeric columns where header / value
    /// width is bounded (e.g. "CPU%" header + "999.9" → ~7 cells).
    Px(f64),
    /// Proportional share of leftover space after the Px columns
    /// are taken.  Two `Flex(1.0)` cols split the rest 50/50; one
    /// `Flex(2.0)` next to one `Flex(1.0)` gives 2:1.
    Flex(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir { Asc, Desc }

/// Differentiates section-header rows (no selection, heavier text)
/// from regular data rows.  Tables without sections only emit `Data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Normal selectable row.
    Data,
    /// Section / pane / group header — drawn with `style.section_fg`,
    /// not selectable, no selection highlight.
    Section,
}

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub header: String,
    pub width: ColumnWidth,
    pub align: Alignment,
    /// Current sort direction, if this column drives the row order.
    /// Caller pre-sorts rows; Table just paints the indicator.
    pub sort: Option<SortDir>,
    /// True = clicking the header sorts by this column (caller
    /// observes via `hit_test_header`).
    pub sortable: bool,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    /// One string per column.  Length should match the columns
    /// slice; missing cells render blank.
    pub cells: Vec<String>,
    /// Indentation depth for tree-style display — shifts the FIRST
    /// column's text by `depth * style.indent_px`.  Other columns
    /// unaffected.
    pub depth: u8,
    pub kind: RowKind,
}

#[derive(Debug, Clone, Copy)]
pub struct TableStyle {
    pub header_h: f64,
    pub row_h: f64,
    pub indent_px: f64,
    pub col_pad: f64,
    pub header_bg: [f32; 4],
    pub header_fg: [f32; 4],
    /// 1-px line under the header band.
    pub header_separator: [f32; 4],
    pub row_bg: [f32; 4],
    /// Alternate row BG for zebra striping; equal to `row_bg` to
    /// disable stripes.
    pub row_bg_alt: [f32; 4],
    pub row_bg_selected: [f32; 4],
    pub row_fg: [f32; 4],
    pub row_fg_selected: [f32; 4],
    pub section_fg: [f32; 4],
}

impl Default for TableStyle {
    fn default() -> Self {
        Self {
            header_h: 24.0,
            row_h: 22.0,
            indent_px: 12.0,
            col_pad: 8.0,
            header_bg: [0.12, 0.13, 0.16, 1.0],
            header_fg: [0.55, 0.60, 0.66, 1.0],
            header_separator: [0.20, 0.22, 0.26, 1.0],
            row_bg: [0.0; 4], // transparent
            row_bg_alt: [0.0; 4],
            row_bg_selected: [0.18, 0.28, 0.48, 1.0],
            row_fg: [0.78, 0.82, 0.87, 1.0],
            row_fg_selected: [0.96, 0.97, 0.99, 1.0],
            section_fg: [0.85, 0.88, 0.92, 1.0],
        }
    }
}

pub struct Table<'a> {
    /// Total bounding rect: header + body (no scrollbar painted).
    pub rect: Rect,
    pub columns: &'a [TableColumn],
    pub rows: &'a [TableRow],
    pub style: TableStyle,
    /// Currently selected row.  None = no highlight.  Section rows
    /// can't be selected (caller skips them when computing).
    pub selected: Option<usize>,
    /// Body scroll offset in physical px.  Header stays sticky.
    pub scroll_y: f64,
    pub show_header: bool,
}

impl<'a> Table<'a> {
    /// Header band rect (zero-height when `show_header == false`).
    pub fn header_rect(&self) -> Rect {
        if !self.show_header {
            return Rect { x: self.rect.x, y_top: self.rect.y_top, w: self.rect.w, h: 0.0 };
        }
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top,
            w: self.rect.w,
            h: self.style.header_h,
        }
    }

    /// Body viewport rect (everything below the header).
    pub fn body_rect(&self) -> Rect {
        let head = self.header_rect().h;
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top + head,
            w: self.rect.w,
            h: (self.rect.h - head).max(0.0),
        }
    }

    /// Solve column rects via the Px/Flex layout.  Returned widths
    /// are `(x, w)` pairs in physical pixels; height is the table's
    /// header_h or row_h depending on the row context.
    pub fn column_x_widths(&self) -> Vec<(f64, f64)> {
        let mut fixed_total = 0.0;
        let mut flex_total = 0.0;
        for col in self.columns {
            match col.width {
                ColumnWidth::Px(w) => fixed_total += w,
                ColumnWidth::Flex(f) => flex_total += f,
            }
        }
        let available = (self.rect.w - fixed_total).max(0.0);
        let mut out = Vec::with_capacity(self.columns.len());
        let mut cx = self.rect.x;
        for col in self.columns {
            let w = match col.width {
                ColumnWidth::Px(w) => w,
                ColumnWidth::Flex(f) => {
                    if flex_total > 0.0 { available * (f / flex_total) } else { 0.0 }
                }
            };
            out.push((cx, w));
            cx += w;
        }
        out
    }

    /// Top-left y of the i-th visible row WITH scroll applied.  Rows
    /// outside the body rect are still returned (caller decides to
    /// skip or clip).
    fn row_y(&self, i: usize) -> f64 {
        self.body_rect().y_top + (i as f64) * self.style.row_h - self.scroll_y
    }

    /// Rect of the i-th data row at its current scroll position.
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x,
            y_top: self.row_y(i),
            w: self.rect.w,
            h: self.style.row_h,
        }
    }

    /// Index of the row containing `(px, py)`, or None.  Section
    /// rows are returned too — caller decides whether to treat
    /// them as selectable.  Out-of-body coords return None.
    pub fn hit_test_row(&self, px: f64, py: f64) -> Option<usize> {
        let body = self.body_rect();
        if !body.contains(px, py) { return None; }
        let rel = py - body.y_top + self.scroll_y;
        let idx = (rel / self.style.row_h).floor() as i64;
        if idx < 0 { return None; }
        let i = idx as usize;
        if i < self.rows.len() { Some(i) } else { None }
    }

    /// Index of the header column containing `(px, py)`, or None.
    /// Only returns Some when the column is `sortable: true` so a
    /// caller can blindly use it for sort-key updates.
    pub fn hit_test_header(&self, px: f64, py: f64) -> Option<usize> {
        let head = self.header_rect();
        if head.h == 0.0 || !head.contains(px, py) { return None; }
        for (i, (cx, cw)) in self.column_x_widths().iter().enumerate() {
            if px >= *cx && px < cx + cw && self.columns[i].sortable {
                return Some(i);
            }
        }
        None
    }

    pub fn paint(&self, p: &mut ViewPainter) {
        if self.columns.is_empty() { return; }
        let col_xw = self.column_x_widths();
        let style = self.style;

        // ── Header band ──────────────────────────────────────────
        if self.show_header && self.style.header_h > 0.0 {
            let head = self.header_rect();
            p.fill_rect(head, style.header_bg);
            // 1-px separator under the header.
            p.fill_rect(
                Rect { x: head.x, y_top: head.y_top + head.h - 1.0,
                       w: head.w, h: 1.0 },
                style.header_separator,
            );
            for (i, (cx, cw)) in col_xw.iter().enumerate() {
                let col = &self.columns[i];
                let inset = self.style.col_pad;
                let txt_rect = Rect {
                    x: cx + inset,
                    y_top: head.y_top,
                    w: (cw - 2.0 * inset).max(0.0),
                    h: head.h,
                };
                // Compose "Header ▲" / "Header ▼" when sorted.
                let glyph = match col.sort {
                    Some(SortDir::Asc) => " ▲",
                    Some(SortDir::Desc) => " ▼",
                    None => "",
                };
                let label = if glyph.is_empty() {
                    col.header.clone()
                } else {
                    format!("{}{}", col.header, glyph)
                };
                p.text_in(txt_rect, &label, style.header_fg, col.align);
            }
        }

        // ── Body rows ────────────────────────────────────────────
        let body = self.body_rect();
        for (i, row) in self.rows.iter().enumerate() {
            let row_rect = self.row_rect(i);
            // Clip rows entirely outside the body.
            if row_rect.y_top + row_rect.h <= body.y_top { continue; }
            if row_rect.y_top >= body.y_top + body.h { break; }

            let is_selected = self.selected == Some(i)
                && row.kind == RowKind::Data;
            // Row BG: selection > stripe > default.
            let bg = if is_selected {
                style.row_bg_selected
            } else if i % 2 == 1 {
                style.row_bg_alt
            } else {
                style.row_bg
            };
            if bg[3] > 0.001 {
                p.fill_rect(row_rect, bg);
            }
            let fg = match row.kind {
                RowKind::Section => style.section_fg,
                RowKind::Data if is_selected => style.row_fg_selected,
                RowKind::Data => style.row_fg,
            };

            for (col_idx, (cx, cw)) in col_xw.iter().enumerate() {
                let col = &self.columns[col_idx];
                let inset_l = style.col_pad
                    + if col_idx == 0 {
                        (row.depth as f64) * style.indent_px
                    } else { 0.0 };
                let inset_r = style.col_pad;
                let txt_rect = Rect {
                    x: cx + inset_l,
                    y_top: row_rect.y_top,
                    w: (cw - inset_l - inset_r).max(0.0),
                    h: row_rect.h,
                };
                let cell = row.cells.get(col_idx).map(|s| s.as_str()).unwrap_or("");
                if !cell.is_empty() {
                    p.text_in(txt_rect, cell, fg, col.align);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(header: &str, width: ColumnWidth, align: Alignment) -> TableColumn {
        TableColumn {
            header: header.into(),
            width,
            align,
            sort: None,
            sortable: false,
        }
    }

    #[test]
    fn flex_columns_share_remaining_width() {
        let cols = vec![
            col("a", ColumnWidth::Px(100.0), Alignment::CenterLeft),
            col("b", ColumnWidth::Flex(1.0), Alignment::CenterLeft),
            col("c", ColumnWidth::Flex(1.0), Alignment::CenterRight),
        ];
        let t = Table {
            rect: Rect { x: 0.0, y_top: 0.0, w: 500.0, h: 100.0 },
            columns: &cols,
            rows: &[],
            style: TableStyle::default(),
            selected: None,
            scroll_y: 0.0,
            show_header: true,
        };
        let xw = t.column_x_widths();
        assert_eq!(xw[0].0, 0.0);
        assert_eq!(xw[0].1, 100.0);
        assert!((xw[1].1 - 200.0).abs() < 1e-6);
        assert!((xw[2].1 - 200.0).abs() < 1e-6);
    }

    #[test]
    fn hit_test_row_skips_scrolled_out() {
        let cols = vec![col("a", ColumnWidth::Flex(1.0), Alignment::CenterLeft)];
        let rows: Vec<TableRow> = (0..10).map(|i| TableRow {
            cells: vec![format!("row{i}")],
            depth: 0, kind: RowKind::Data,
        }).collect();
        let t = Table {
            rect: Rect { x: 0.0, y_top: 0.0, w: 200.0, h: 100.0 },
            columns: &cols,
            rows: &rows,
            style: TableStyle::default(),
            selected: None,
            scroll_y: 0.0,
            show_header: false,
        };
        // Click at y=10 → row index 0 with default row_h=22.
        let body_top = t.body_rect().y_top;
        assert_eq!(t.hit_test_row(50.0, body_top + 10.0), Some(0));
        // Click way past last row → None.
        assert_eq!(t.hit_test_row(50.0, body_top + 1000.0), None);
    }

    #[test]
    fn header_hit_test_filters_unsortable() {
        let cols = vec![
            TableColumn { header: "a".into(), width: ColumnWidth::Flex(1.0),
                align: Alignment::CenterLeft, sort: None, sortable: true },
            TableColumn { header: "b".into(), width: ColumnWidth::Flex(1.0),
                align: Alignment::CenterLeft, sort: None, sortable: false },
        ];
        let t = Table {
            rect: Rect { x: 0.0, y_top: 0.0, w: 200.0, h: 100.0 },
            columns: &cols, rows: &[],
            style: TableStyle::default(),
            selected: None, scroll_y: 0.0, show_header: true,
        };
        let head = t.header_rect();
        let hit_a = t.hit_test_header(50.0, head.y_top + 5.0);
        let hit_b = t.hit_test_header(150.0, head.y_top + 5.0);
        assert_eq!(hit_a, Some(0));   // sortable
        assert_eq!(hit_b, None);      // not sortable
    }
}
