//! Scrollback pages: a slice of history serialised for a reader that
//! pages through it, the decoder for that body, and pushing a decoded
//! line back into history.

use super::Terminal;
use super::snapshot_clusters::plausible_cols;
use super::snapshot_wire::{CELL_BYTES, read_attrs, read_u8, read_u32, serialize_attrs};
use crate::grid::Cell;
use std::io::{self, Cursor};

impl Terminal {
    /// RFC-002 §8: serialize a contiguous slice of the scrollback into
    /// the `ScrollbackPage` body format.
    ///
    /// `line_start` indexes the scrollback ring from the oldest live
    /// line (0 = bottom of history, i.e. just above the visible grid
    /// is the highest index — same convention as `Grid::scrollback_*`).
    /// `count` is a request cap; the returned `line_count` is the
    /// number of lines actually serialised (may be < count when the
    /// request hits the end of scrollback).
    ///
    /// Wire format (per line — outer framing handled by
    /// `shelld_proto::encode_scrollback_page`):
    ///
    /// ```text
    /// repeat line_count times:
    ///   [line_cols u32 LE]                — width of THIS line
    ///   [wrapped  u8]                     — 1 if this row is a
    ///                                       continuation of the previous
    ///                                       row (autowrap), 0 if it
    ///                                       started a fresh logical line
    ///   [cells: line_cols × 13 bytes]     — same Cell encoding as
    ///                                       `serialize_snapshot`
    /// ```
    ///
    /// Without the `wrapped` flag the receiver's later `Grid::resize`
    /// reflow can't tell hard newlines from autowrap continuations,
    /// and long lines stay chopped at the narrowest width the grid
    /// ever saw.
    pub fn serialize_scrollback_page(&self, line_start: u32, count: u32) -> (u32, Vec<u8>) {
        let lines = self
            .grid
            .scrollback_read_page(line_start as usize, count as usize);
        let line_count = lines.len() as u32;
        let body_bytes: usize = lines
            .iter()
            .map(|(l, _)| 4 + 1 + l.len() * CELL_BYTES)
            .sum();
        let mut out = Vec::with_capacity(body_bytes);
        for (line, wrapped) in &lines {
            out.extend_from_slice(&(line.len() as u32).to_le_bytes());
            out.push(if *wrapped { 1 } else { 0 });
            for cell in line {
                out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                out.extend_from_slice(&serialize_attrs(cell.attrs));
            }
        }
        (line_count, out)
    }

    /// RFC-002 §8 (step 8c): push a historic line (chronologically
    /// older than every line already in scrollback) into the tail of
    /// the scrollback ring.  Thin wrapper around
    /// `Grid::push_historic_scrollback_line`; same ordering caveat —
    /// caller must apply history in oldest-first order before any
    /// live data has scrolled off the visible grid.  `wrapped` is
    /// the line's autowrap-continuation flag from L4 (see the
    /// wire-format doc on `serialize_scrollback_page`).
    pub fn push_historic_line(&mut self, line: &[Cell], wrapped: bool) {
        self.grid.push_historic_scrollback_line(line, wrapped);
    }

    /// Inverse of `serialize_scrollback_page`.  Static — no `&self`
    /// because the decoder doesn't touch terminal state; callers
    /// (L3 publish-cache) decide what to do with the lines.
    ///
    /// Wire-compat: tries the current "with-wrapped" layout first
    /// (each line = `[cols u32][wrapped u8][cells]`).  If it doesn't
    /// consume exactly `body.len()` bytes, falls back to the legacy
    /// layout (`[cols u32][cells]`, no wrapped byte) emitted by L4
    /// shelld 0.2.6 and earlier — in that case every row is reported
    /// `wrapped = false`, so resize-time reflow won't glue continuation
    /// rows but the cell contents are correct.  Once every running
    /// L4 has been upgraded past 0.2.7 the legacy branch can be
    /// deleted.
    pub fn decode_scrollback_page_body(
        line_count: u32,
        body: &[u8],
    ) -> io::Result<Vec<(Vec<Cell>, bool)>> {
        if let Ok((out, consumed)) = decode_with_wrapped(line_count, body) {
            if consumed == body.len() {
                return Ok(out);
            }
        }
        decode_legacy_no_wrapped(line_count, body)
    }
}

fn decode_with_wrapped(
    line_count: u32,
    body: &[u8],
) -> io::Result<(Vec<(Vec<Cell>, bool)>, usize)> {
    let mut cur = Cursor::new(body);
    let mut out = Vec::with_capacity(line_count as usize);
    for _ in 0..line_count {
        let line_cols = read_u32(&mut cur)? as usize;
        let wrapped = read_u8(&mut cur)? != 0;
        plausible_cols(&cur, line_cols)?;
        let mut line = Vec::with_capacity(line_cols);
        for _ in 0..line_cols {
            let ch_u = read_u32(&mut cur)?;
            let attrs = read_attrs(&mut cur)?;
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            line.push(Cell { ch, attrs });
        }
        out.push((line, wrapped));
    }
    Ok((out, cur.position() as usize))
}

fn decode_legacy_no_wrapped(line_count: u32, body: &[u8]) -> io::Result<Vec<(Vec<Cell>, bool)>> {
    let mut cur = Cursor::new(body);
    let mut out = Vec::with_capacity(line_count as usize);
    for _ in 0..line_count {
        let line_cols = read_u32(&mut cur)? as usize;
        plausible_cols(&cur, line_cols)?;
        let mut line = Vec::with_capacity(line_cols);
        for _ in 0..line_cols {
            let ch_u = read_u32(&mut cur)?;
            let attrs = read_attrs(&mut cur)?;
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            line.push(Cell { ch, attrs });
        }
        out.push((line, false));
    }
    Ok(out)
}
