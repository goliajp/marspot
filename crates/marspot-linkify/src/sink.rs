//! Buffers a scan reuses between calls, and the sink its matches are written to.

use crate::line::LineSegment;
use crate::path::unquote_path_into;
use crate::{LinkKind, LinkRange};

/// Every buffer a scan needs, kept between calls so that a warm scan
/// allocates nothing.  One per pane:
/// [`scan_visible_links_into`](crate::scan_visible_links_into) leaves
/// no state in it that changes a result, only capacity.
#[derive(Default)]
pub struct LinkScratch {
    pub(crate) chars: Vec<char>,
    pub(crate) col_map: Vec<u16>,
    pub(crate) segments: Vec<LineSegment>,
    pub(crate) rule_cols: Vec<(u16, u16)>,
    pub(crate) table: TableScratch,
    pub(crate) text: TextBufs,
    /// Target strings detached from a reused slot, waiting for the
    /// next link that has one.
    pub(crate) targets: Vec<String>,
    /// Ranges past the end of the last result, kept for their strings.
    pub(crate) spare: Vec<LinkRange>,
}

#[derive(Default)]
pub(crate) struct TableScratch {
    pub(crate) block_rows: Vec<u16>,
    pub(crate) shared: Vec<u16>,
    pub(crate) next: Vec<u16>,
    pub(crate) cell: LineBufs,
}

#[derive(Default)]
pub(crate) struct LineBufs {
    pub(crate) chars: Vec<char>,
    pub(crate) col_map: Vec<u16>,
    pub(crate) segments: Vec<LineSegment>,
}

/// Strings a candidate is assembled in before it is asked about or
/// emitted.
#[derive(Default)]
pub(crate) struct TextBufs {
    pub(crate) span: String,
    pub(crate) path: String,
    pub(crate) joined: String,
}

pub(crate) fn collect_into(out: &mut String, chars: &[char]) {
    out.clear();
    out.extend(chars);
}

/// `chars` as drawn into `bufs.span`, unquoted into `bufs.path`.
pub(crate) fn unquote_span(bufs: &mut TextBufs, chars: &[char]) {
    collect_into(&mut bufs.span, chars);
    unquote_path_into(&mut bufs.path, &bufs.span);
}

/// The scan's output: `out[..len]` is the result so far, and
/// `out[len..]` are ranges free to be overwritten.
///
/// Ranges are never dropped mid-scan, only moved past `len`, so their
/// strings stay allocated for the next push — and for the next frame.
pub(crate) struct Sink<'a> {
    pub(crate) out: &'a mut Vec<LinkRange>,
    pub(crate) len: usize,
    /// Target strings no range is using at the moment.
    pub(crate) targets: &'a mut Vec<String>,
}

impl Sink<'_> {
    pub(crate) fn live(&self) -> &[LinkRange] {
        &self.out[..self.len]
    }

    pub(crate) fn live_mut(&mut self) -> &mut [LinkRange] {
        &mut self.out[..self.len]
    }

    /// `Vec::remove` on the result, keeping the removed range.
    pub(crate) fn remove(&mut self, i: usize) {
        self.out[i..self.len].rotate_left(1);
        self.len -= 1;
    }

    pub(crate) fn push(
        &mut self,
        row: u16,
        col_start: u16,
        col_end: u16,
        kind: LinkKind,
        text: &str,
        target: Option<&str>,
    ) {
        let at = self.len;
        if at == self.out.len() {
            self.out.push(LinkRange {
                row,
                col_start,
                col_end,
                kind,
                text: String::new(),
                target: None,
            });
        } else {
            // Best fit, not the next in line: the free ranges come in
            // the order the last frame left them, and the next text in
            // line is rarely the one whose string it already holds.
            // Smallest capacity that fits never spends a string a later
            // text needs, so content that repeats never grows one.
            let free = &self.out[at..];
            let pick = smallest_fit(free.iter().map(|l| l.text.capacity()), text.len())
                .unwrap_or_else(|| largest(free.iter().map(|l| l.text.capacity())));
            self.out.swap(at, at + pick);
        }
        self.len += 1;
        let slot = &mut self.out[at];
        slot.row = row;
        slot.col_start = col_start;
        slot.col_end = col_end;
        slot.kind = kind;
        slot.text.clear();
        slot.text.push_str(text);
        let Some(t) = target else {
            if let Some(s) = slot.target.take() {
                self.targets.push(s);
            }
            return;
        };
        let mut fresh = false;
        if slot.target.as_ref().is_none_or(|s| s.capacity() < t.len()) {
            if let Some(k) = smallest_fit(self.targets.iter().map(String::capacity), t.len()) {
                let s = self.targets.swap_remove(k);
                if let Some(old) = slot.target.replace(s) {
                    self.targets.push(old);
                }
            } else if slot.target.is_none() {
                match self.targets.pop() {
                    Some(s) => slot.target = Some(s),
                    None => fresh = true,
                }
            }
        }
        let s = slot.target.get_or_insert_with(String::new);
        s.clear();
        s.push_str(t);
        if fresh {
            // A new string: make room to park every one in existence,
            // so parking one never grows the pool.
            let total = self.targets.len() + self.out.iter().filter(|l| l.target.is_some()).count();
            self.targets.reserve(total - self.targets.len());
        }
    }
}

/// Index of the smallest capacity that is at least `need`.
fn smallest_fit(caps: impl Iterator<Item = usize>, need: usize) -> Option<usize> {
    caps.enumerate()
        .filter(|&(_, c)| c >= need)
        .min_by_key(|&(_, c)| c)
        .map(|(k, _)| k)
}

fn largest(caps: impl Iterator<Item = usize>) -> usize {
    caps.enumerate().max_by_key(|&(_, c)| c).map_or(0, |(k, _)| k)
}
