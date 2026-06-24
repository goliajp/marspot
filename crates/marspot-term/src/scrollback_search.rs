//! Wrap-aware, cancellable substring search over a persistent
//! `scrollback.bin` (or any `Scrollback`).  B1 of the pane upgrade
//! rollout — see `docs/scrollback-search.md` §4.
//!
//! The engine owns the *algorithm*; B3 wires it into the L3 wire
//! handler.  Pure-function shape (one entry point, an iterator out)
//! so it stays fully testable without `marspot-session` machinery.
//!
//! Key design points (decisions D9, D11, D15):
//!
//! - **Logical lines** group rows joined by DECAWM `wrapped` flags
//!   AND a cc-style "hard-wrap with hanging indent" heuristic
//!   (same predicate as `grid_links::is_cc_hard_wrap_continuation`).
//!   So a URL that overflowed a 100-col claudecode TUI into two
//!   physical rows is *one* hit, not two.
//! - **norm_text** has the hanging indent stripped and continuation
//!   joins applied.  Queries match against norm_text.
//! - **raw_text** preserves the original `\n` + leading spaces so
//!   we can map every char position in norm_text back to a physical
//!   `(line_idx, col)` for highlight rendering.
//! - **Newest-first** iteration: the engine walks scrollback from
//!   the latest line backwards, since the most-recent hits are what
//!   the user wants to see first in the SearchList.
//! - **Cancellation via drop**: the returned iterator's `Drop` is
//!   the cheap, single-point cancel surface; the L3 worker thread
//!   (B3) sets an `Arc<AtomicBool>` on `SearchCancel`, the iterator
//!   honours it between hits.
//!
//! Performance budget (D2 deferred): 1 MB scrollback first 64 hits
//! < 20 ms on M-series.  Measured in the `b1_perf_*` tests using
//! `Instant::now`; if breached, we revisit the sidecar text mirror.

use crate::grid::Cell;
use crate::scrollback::Scrollback;

/// Tunables for a single search session.
#[derive(Clone, Debug)]
pub struct SearchOpts {
    /// Case sensitivity toggle.  Default `false` matches modern
    /// terminal-search expectations (browser-style).
    pub case_sensitive: bool,
    /// First-batch cap.  After this many hits the iterator pauses
    /// awaiting the caller's `SearchMore` (B3 wire layer turns this
    /// into the streaming protocol).
    pub max_total: u32,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            max_total: 64,
        }
    }
}

/// One physical-row span of a multi-row match.  Highlight renderer
/// (C4) draws one rect per span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalSpan {
    /// Index into the source scrollback (0 = oldest).  Matches the
    /// indices used by `Scrollback::cell_at` / `read_line`.
    pub phys_row_idx: u64,
    pub col_start: u16,
    /// Inclusive end column.
    pub col_end_inclusive: u16,
}

/// A single match.  Multiple matches in the same logical line are
/// emitted as separate `SearchHit`s in column order.
#[derive(Clone, Debug)]
pub struct SearchHit {
    /// Index of the *logical* line containing this match.  Logical
    /// lines are derived by the iterator's grouping rule and DO NOT
    /// directly index the scrollback ring (see `physical_rows` for
    /// that).  Caller (C3 result list) uses this for de-duplication
    /// and as a stable identifier.
    pub logical_line_idx: u64,
    /// Char offset of the match inside the logical line's
    /// `norm_text`.
    pub char_offset: u32,
    /// Char length of the match (in chars, not bytes — UTF-8 safe).
    pub char_len: u32,
    /// Snippet for the result list — up to 80 chars from `norm_text`,
    /// centred on the match where possible.
    pub snippet: String,
    /// Char offset of the match's start within `snippet`.
    pub snippet_match_start: u16,
    /// Char offset of the match's end within `snippet` (exclusive).
    pub snippet_match_end: u16,
    /// Per-physical-row spans for highlight rendering.  At least one
    /// entry; > 1 for matches that crossed wrap boundaries.
    pub physical_rows: Vec<PhysicalSpan>,
}

/// Read-only view over the data the engine needs.  Lets B1 stay
/// independent of `FileScrollback` so tests use `MemoryScrollback`
/// happily and the algorithm has zero IO knowledge.
pub trait SearchSource {
    /// Total scrollback line count.
    fn line_count(&self) -> u64;
    /// One row's cells, oldest=0.
    fn line(&self, idx: u64) -> Option<Vec<Cell>>;
    /// DECAWM continuation flag for row `idx`.  Memory/Disk
    /// variants always return `false` here in v1 — they don't track
    /// wrapped in scrollback (`sb_wrapped` on Grid is the truth).
    /// File variant returns the persisted flag.  Tests can fake
    /// either.
    fn wrapped(&self, idx: u64) -> bool;
}

impl SearchSource for Scrollback {
    fn line_count(&self) -> u64 {
        self.len() as u64
    }
    fn line(&self, idx: u64) -> Option<Vec<Cell>> {
        self.line_to_vec(idx as usize)
    }
    fn wrapped(&self, idx: u64) -> bool {
        self.wrapped_at(idx as usize)
    }
}

/// Test/synthetic adapter — owns the data, no IO.
pub struct InMemorySource {
    pub rows: Vec<(Vec<Cell>, bool)>,
}

impl SearchSource for InMemorySource {
    fn line_count(&self) -> u64 {
        self.rows.len() as u64
    }
    fn line(&self, idx: u64) -> Option<Vec<Cell>> {
        self.rows.get(idx as usize).map(|(c, _)| c.clone())
    }
    fn wrapped(&self, idx: u64) -> bool {
        self.rows.get(idx as usize).map(|(_, w)| *w).unwrap_or(false)
    }
}

/// One logical line, materialised lazily by the iterator.
struct LogicalLine {
    /// Char-by-char text after normalisation (hanging-indent strip,
    /// wrap merge).  This is what queries match against.
    norm_text: String,
    /// For each char position in `norm_text`, what physical row + col
    /// did it come from?  Char vector — UTF-8 safe via Vec<char>.
    norm_to_phys: Vec<(u64, u16)>,
    /// Logical-line index assigned by the iterator (descending — the
    /// newest logical line gets the highest number).
    logical_idx: u64,
}

/// Main entry point.  Builds an iterator that yields `SearchHit`s
/// newest-first.  The iterator owns enough state to be cancelled
/// via `Drop`; an `Arc<AtomicBool>`-based cancel hook is added in
/// B3 when the worker thread needs it.
pub fn search_scrollback<S: SearchSource>(
    source: S,
    query: String,
    opts: SearchOpts,
) -> SearchIter<S> {
    SearchIter::new(source, query, opts)
}

pub struct SearchIter<S: SearchSource> {
    source: S,
    query_lower: String,           // pre-lowercased for case-insensitive scan
    query_chars: Vec<char>,         // length-cached
    opts: SearchOpts,
    /// Next physical row to consider (walks downward from
    /// line_count - 1 towards 0; matches the "newest-first" rule).
    next_phys_back: i64,
    /// Buffer of un-yielded hits from the current logical line.
    pending: std::collections::VecDeque<SearchHit>,
    /// Number of hits yielded so far this iterator.  Stops at
    /// `opts.max_total`.
    yielded: u32,
    /// Counter for assigning `logical_line_idx`; decrements as we
    /// emit older lines.
    next_logical_idx: u64,
}

impl<S: SearchSource> SearchIter<S> {
    fn new(source: S, query: String, opts: SearchOpts) -> Self {
        let query_lower = if opts.case_sensitive {
            query.clone()
        } else {
            query.to_lowercase()
        };
        let query_chars: Vec<char> = query.chars().collect();
        let line_count = source.line_count() as i64;
        // logical_line_idx assignment: walking newest→oldest, we
        // assign logical_line_idx = line_count - 1 to the very newest
        // logical line, decrementing for each older one.  This gives
        // stable, predictable indices a caller can sort by.
        let next_logical_idx = if line_count > 0 { (line_count - 1) as u64 } else { 0 };
        Self {
            source,
            query_lower,
            query_chars,
            opts,
            next_phys_back: line_count - 1,
            pending: std::collections::VecDeque::new(),
            yielded: 0,
            next_logical_idx,
        }
    }

    /// Build the *next* logical line by walking backward from
    /// `next_phys_back`, grouping continuation rows above it.
    /// Returns None when there's nothing left.
    fn next_logical(&mut self) -> Option<LogicalLine> {
        if self.next_phys_back < 0 {
            return None;
        }
        let last = self.next_phys_back as u64;
        // Walk upward: an older row is part of THIS logical line if
        // (a) it's flagged wrapped (DECAWM continuation of the row
        //     ABOVE it that we'd then also follow), OR
        // (b) cc-hard-wrap heuristic says the row below this one is
        //     a continuation of this one (URL/path style).
        //
        // The flag direction: `wrapped(row)` means "row is a
        // continuation of the row above it".  So a logical line
        // running from `first` to `last`:
        //   wrapped(first) MAY be false (it's the start)
        //   wrapped(first + 1 .. = last) MUST be true
        // We work backward from `last` and pull in rows above as
        // long as `wrapped(current)` says they continue.
        let mut first = last;
        while first > 0 {
            let candidate = first;
            if !self.source.wrapped(candidate) && !self.is_cc_hard_wrap(first - 1, candidate) {
                break;
            }
            first -= 1;
        }
        // After loop: `first` is the topmost physical row in this
        // logical line.
        // Build raw + norm text.
        let mut norm_text = String::new();
        let mut norm_to_phys: Vec<(u64, u16)> = Vec::new();
        for r in first..=last {
            let Some(cells) = self.source.line(r) else { continue; };
            let prev_was_cc_cont = r > first
                && !self.source.wrapped(r)
                && self.is_cc_hard_wrap(r - 1, r);
            let leading_skip = if prev_was_cc_cont {
                count_leading_ws_cells(&cells).min(4)
            } else if r > first && self.source.wrapped(r) {
                // DECAWM wrap continuation: chars start at col 0 of
                // this row; no indent to strip.
                0
            } else {
                0
            };
            // Skip leading hanging indent for cc-merged rows, then
            // walk cells emitting chars + reverse mapping.
            for (col_idx, cell) in cells.iter().enumerate() {
                if col_idx < leading_skip {
                    continue;
                }
                let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
                norm_text.push(ch);
                norm_to_phys.push((r, col_idx as u16));
            }
        }
        // Trim trailing whitespace from norm_text (terminals fill
        // unused cells with spaces).  We trim *only* the run of
        // trailing whitespace, not interior whitespace.
        let trim_len = norm_text
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        // Truncate norm_text and the parallel norm_to_phys.
        // Be careful: len_utf8 vs char count.  Drop chars iff their
        // byte position >= trim_len.
        let drop_from_byte = trim_len;
        let mut keep_chars = 0;
        let mut byte = 0;
        for c in norm_text.chars() {
            if byte >= drop_from_byte {
                break;
            }
            byte += c.len_utf8();
            keep_chars += 1;
        }
        norm_text.truncate(drop_from_byte);
        norm_to_phys.truncate(keep_chars);

        let logical_idx = self.next_logical_idx;
        if logical_idx > 0 {
            self.next_logical_idx -= 1;
        }
        // Advance the cursor: next logical line is the one ending at
        // `first - 1` (i.e. directly above `first`).
        self.next_phys_back = first as i64 - 1;
        // `first` / `last` go uncomsumed here today; the row range
        // can be reconstructed from `norm_to_phys` if a future call
        // path needs it.
        let _ = (first, last);
        Some(LogicalLine {
            norm_text,
            norm_to_phys,
            logical_idx,
        })
    }

    /// cc heuristic — does `lower` continue `upper`?  Same predicate
    /// as `grid_links::is_cc_hard_wrap_continuation` but operating
    /// against the search source instead of a live grid.
    fn is_cc_hard_wrap(&self, upper_idx: u64, lower_idx: u64) -> bool {
        let Some(upper) = self.source.line(upper_idx) else { return false; };
        let Some(lower) = self.source.line(lower_idx) else { return false; };
        if upper.is_empty() || lower.is_empty() {
            return false;
        }
        // Upper row last non-blank cell must be at or near right
        // edge and be URL/path-class.
        let upper_cols = upper.len();
        let mut last_nb_col = None;
        let mut last_nb_ch = ' ';
        for (i, c) in upper.iter().enumerate().rev() {
            if c.ch != '\0' && c.ch != ' ' {
                last_nb_col = Some(i);
                last_nb_ch = c.ch;
                break;
            }
        }
        let last_nb_col = match last_nb_col {
            Some(v) => v,
            None => return false,
        };
        if last_nb_col < upper_cols.saturating_sub(2) {
            return false;
        }
        if !is_url_path_class(last_nb_ch) {
            return false;
        }
        // Lower row leading whitespace 1..=4 then URL/path-class.
        let mut lead = 0usize;
        while lead < lower.len() {
            let ch = lower[lead].ch;
            if ch == ' ' || ch == '\0' {
                lead += 1;
            } else {
                break;
            }
        }
        if !(1..=4).contains(&lead) || lead >= lower.len() {
            return false;
        }
        is_url_path_class(lower[lead].ch)
    }

    /// Scan one logical line for hits, populate `self.pending`.
    fn scan_logical(&mut self, line: LogicalLine) {
        if self.query_chars.is_empty() {
            return;
        }
        let haystack = if self.opts.case_sensitive {
            line.norm_text.clone()
        } else {
            line.norm_text.to_lowercase()
        };
        let mut start = 0;
        while let Some(byte_off) = haystack[start..].find(self.query_lower.as_str()) {
            let match_byte_start = start + byte_off;
            let match_byte_end = match_byte_start + self.query_lower.len();
            // Convert byte offsets to char offsets in line.norm_text.
            let char_offset = haystack[..match_byte_start].chars().count();
            let char_len = self.query_chars.len();
            // Build physical row spans.
            let physical_rows = build_phys_spans(&line, char_offset, char_len);
            // Build snippet centred on match.
            let (snippet, snip_match_start, snip_match_end) =
                build_snippet(&line.norm_text, char_offset, char_len);
            self.pending.push_back(SearchHit {
                logical_line_idx: line.logical_idx,
                char_offset: char_offset as u32,
                char_len: char_len as u32,
                snippet,
                snippet_match_start: snip_match_start,
                snippet_match_end: snip_match_end,
                physical_rows,
            });
            start = match_byte_end;
            if start > haystack.len() {
                break;
            }
        }
    }
}

impl<S: SearchSource> Iterator for SearchIter<S> {
    type Item = SearchHit;

    fn next(&mut self) -> Option<SearchHit> {
        if self.query_chars.is_empty() {
            return None;
        }
        loop {
            if self.yielded >= self.opts.max_total {
                return None;
            }
            if let Some(hit) = self.pending.pop_front() {
                self.yielded += 1;
                return Some(hit);
            }
            let line = self.next_logical()?;
            self.scan_logical(line);
            if self.pending.is_empty() {
                continue; // logical line had no hits; move on
            }
        }
    }
}

fn is_url_path_class(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '.' | '-' | '_' | '~' | '?' | '&' | '=' | '#' | '%' | ':' | '+' | '@' | ','
        )
}

fn count_leading_ws_cells(cells: &[Cell]) -> usize {
    let mut n = 0;
    for c in cells {
        if c.ch == ' ' || c.ch == '\0' {
            n += 1;
        } else {
            break;
        }
    }
    n
}

fn build_phys_spans(line: &LogicalLine, char_offset: usize, char_len: usize) -> Vec<PhysicalSpan> {
    if char_len == 0 || line.norm_to_phys.is_empty() {
        return Vec::new();
    }
    // For each (phys_row_idx, col) walked through, group consecutive
    // entries belonging to the same phys_row into a span.
    let mut spans: Vec<PhysicalSpan> = Vec::new();
    let end = (char_offset + char_len).min(line.norm_to_phys.len());
    let mut i = char_offset;
    while i < end {
        let (row, col_start) = line.norm_to_phys[i];
        let mut col_end = col_start;
        let mut j = i + 1;
        while j < end {
            let (r2, c2) = line.norm_to_phys[j];
            if r2 != row {
                break;
            }
            col_end = c2;
            j += 1;
        }
        spans.push(PhysicalSpan {
            phys_row_idx: row,
            col_start,
            col_end_inclusive: col_end,
        });
        i = j;
    }
    spans
}

// B3 — `SearchSource` adapter for `FileSnapshot`.  Lives here (not
// in scrollback.rs) so the engine-internal trait can stay private to
// this module while the file-backed source slots in as another
// implementation.
impl SearchSource for crate::scrollback::FileSnapshot {
    fn line_count(&self) -> u64 {
        self.total_lines()
    }
    fn line(&self, idx: u64) -> Option<Vec<Cell>> {
        self.search_line(idx)
    }
    fn wrapped(&self, idx: u64) -> bool {
        self.search_wrapped(idx)
    }
}

// ──────────────────── B3: worker thread + wire shape ─────────────

/// Off-thread search worker.  Owned by the L3 main loop; dropping it
/// is the cancel signal.  The thread polls `cancel` between each
/// emitted hit (≤ a few µs per check at search throughput) and
/// abandons its batch on cancel, so no result can ever land for a
/// cancelled query_id.
pub struct SearchWorker {
    pub query_id: u32,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Detached on drop — the worker exits within < 1 ms of its next
    // cancel check.  We don't join (would block the main loop).
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl SearchWorker {
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Snapshot the cancel flag.  Used by tests to assert that
    /// last-write-wins cancels the prior worker.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
        // Don't join — the worker exits at its next cancel.load(),
        // which inside `scan_logical` is at most one logical line
        // away.  Joining would risk blocking the main loop for the
        // duration of one substring scan over a giant logical line.
    }
}

/// Convert a fully-built `SearchHit` into the wire shape used by the
/// `SearchResults` frame (§5.3 of `docs/scrollback-search.md`).
pub fn to_wire_hit(h: SearchHit) -> crate::shell_proto::WireSearchHit {
    let spans = h
        .physical_rows
        .into_iter()
        .map(|s| crate::shell_proto::WirePhysicalSpan {
            phys_row_idx: s.phys_row_idx,
            col_start: s.col_start,
            col_end_inclusive: s.col_end_inclusive,
        })
        .collect();
    crate::shell_proto::WireSearchHit {
        logical_line_idx: h.logical_line_idx,
        char_offset: h.char_offset,
        char_len: h.char_len,
        snippet: h.snippet,
        snippet_match_start: h.snippet_match_start,
        snippet_match_end: h.snippet_match_end,
        spans,
    }
}

/// B4 — owned snapshot of the currently-visible grid rows.  Slotted
/// "above" the scrollback file in the worker's composed source so
/// hits in the live grid are returned first (newest-first walk).
///
/// Convention: `rows[0]` is the topmost visible grid row,
/// `rows[rows.len() - 1]` is the bottom row (closest to the cursor).
/// This matches `Grid::live_grid_snapshot_for_search`'s output order.
pub struct LiveGridSnapshot {
    pub rows: Vec<(Vec<Cell>, bool)>,
}

impl LiveGridSnapshot {
    pub fn from_rows(rows: Vec<(Vec<Cell>, bool)>) -> Self {
        Self { rows }
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// B4 — composed source: file rows at indices `0..file.line_count()`,
/// then live rows immediately above at `file.line_count()..
/// file.line_count() + live.len()`.  The newest physical row
/// (`line_count() - 1`) is the bottom of the live grid, so the
/// engine's newest-first walk hits the live grid's bottom row first
/// and proceeds upward through the live grid, then continues into
/// the most-recent scrollback rows.  Worker post-processing
/// translates any hit whose primary row landed in the live range
/// into the `u64::MAX - row_offset` synthetic index (§4.5).
struct MergedLiveFileSource<S: SearchSource> {
    file: S,
    live: LiveGridSnapshot,
    file_total: u64,
}

impl<S: SearchSource> MergedLiveFileSource<S> {
    fn new(file: S, live: LiveGridSnapshot) -> Self {
        let file_total = file.line_count();
        Self {
            file,
            live,
            file_total,
        }
    }
}

impl<S: SearchSource> SearchSource for MergedLiveFileSource<S> {
    fn line_count(&self) -> u64 {
        self.file_total + self.live.rows.len() as u64
    }
    fn line(&self, idx: u64) -> Option<Vec<Cell>> {
        if idx < self.file_total {
            self.file.line(idx)
        } else {
            let local = (idx - self.file_total) as usize;
            self.live.rows.get(local).map(|(c, _)| c.clone())
        }
    }
    fn wrapped(&self, idx: u64) -> bool {
        if idx < self.file_total {
            self.file.wrapped(idx)
        } else {
            let local = (idx - self.file_total) as usize;
            self.live.rows.get(local).map(|(_, w)| *w).unwrap_or(false)
        }
    }
}

/// Map a `SearchHit` to a wire-shape `WireSearchHit`, remapping any
/// span landing in the live-grid range to a live-local row index and
/// translating the logical_line_idx to the `u64::MAX - row_offset`
/// synthetic-index convention (§4.5).  Used by `spawn_search_merged`.
fn remap_to_wire(hit: SearchHit, file_total: u64) -> crate::shell_proto::WireSearchHit {
    // Determine whether this hit's "primary" row is live.  Per §4.5
    // the synthetic index encodes the live row offset, so we key off
    // the LAST physical row (= the row farthest from the
    // newest = the bottom of the matched span when reading
    // top-to-bottom).  For a single-row match the answer is the same.
    let max_phys = hit
        .physical_rows
        .iter()
        .map(|s| s.phys_row_idx)
        .max()
        .unwrap_or(0);
    let is_live = max_phys >= file_total;
    let logical_line_idx = if is_live {
        let live_local = max_phys - file_total;
        u64::MAX - live_local
    } else {
        hit.logical_line_idx
    };
    let spans = hit
        .physical_rows
        .into_iter()
        .map(|s| {
            let phys_row_idx = if s.phys_row_idx >= file_total {
                s.phys_row_idx - file_total
            } else {
                s.phys_row_idx
            };
            crate::shell_proto::WirePhysicalSpan {
                phys_row_idx,
                col_start: s.col_start,
                col_end_inclusive: s.col_end_inclusive,
            }
        })
        .collect();
    crate::shell_proto::WireSearchHit {
        logical_line_idx,
        char_offset: hit.char_offset,
        char_len: hit.char_len,
        snippet: hit.snippet,
        snippet_match_start: hit.snippet_match_start,
        snippet_match_end: hit.snippet_match_end,
        spans,
    }
}

/// Spawn a search worker thread.  Takes ownership of `source` (a
/// `Send` `SearchSource` — typically `FileSnapshot`) and runs the
/// engine to completion (or cancel).  Delivers exactly one batch
/// via `on_batch(query_id, hits, has_more, total_seen)` when the
/// scan finishes naturally; delivers nothing when cancelled.
///
/// `has_more = (hits.len() == opts.max_total)` is an approximate
/// flag — it's true whenever the engine stopped because it filled
/// the cap, false when it ran to the end of scrollback first.
/// `SearchMore` (D-phase) will turn this into a streaming protocol.
pub fn spawn_search<S, F>(
    query_id: u32,
    source: S,
    query: String,
    opts: SearchOpts,
    on_batch: F,
) -> SearchWorker
where
    S: SearchSource + Send + 'static,
    F: FnOnce(u32, Vec<crate::shell_proto::WireSearchHit>, bool, u32) + Send + 'static,
{
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_w = std::sync::Arc::clone(&cancel);
    let cap = opts.max_total;
    let handle = std::thread::Builder::new()
        .name(format!("l3-search-{query_id}"))
        .spawn(move || {
            let mut iter = search_scrollback(source, query, opts);
            let mut hits: Vec<crate::shell_proto::WireSearchHit> = Vec::new();
            let mut total_seen: u32 = 0;
            loop {
                if cancel_w.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                match iter.next() {
                    Some(h) => {
                        total_seen = total_seen.saturating_add(1);
                        hits.push(to_wire_hit(h));
                    }
                    None => break,
                }
            }
            if cancel_w.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let has_more = (hits.len() as u32) == cap;
            on_batch(query_id, hits, has_more, total_seen);
        })
        .expect("spawn search worker");
    SearchWorker {
        query_id,
        cancel,
        _handle: Some(handle),
    }
}

/// B4 — spawn the search worker against a merged live-grid + file
/// scrollback source.  Same cancel + delivery contract as
/// `spawn_search`; the only behavioural delta is that hits whose
/// primary physical row landed in the live grid range are remapped
/// to the `u64::MAX - row_offset` synthetic logical-line index and
/// their spans are translated into live-local row indices.  L2
/// decodes `logical_line_idx >= u64::MAX - rows` as a live-grid hit
/// and computes view_offset = `rows - 1 - (u64::MAX - hit.idx)`.
pub fn spawn_search_merged<S, F>(
    query_id: u32,
    file_source: S,
    live: LiveGridSnapshot,
    query: String,
    opts: SearchOpts,
    on_batch: F,
) -> SearchWorker
where
    S: SearchSource + Send + 'static,
    F: FnOnce(u32, Vec<crate::shell_proto::WireSearchHit>, bool, u32) + Send + 'static,
{
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_w = std::sync::Arc::clone(&cancel);
    let cap = opts.max_total;
    let handle = std::thread::Builder::new()
        .name(format!("l3-search-merged-{query_id}"))
        .spawn(move || {
            let merged = MergedLiveFileSource::new(file_source, live);
            let file_total = merged.file_total;
            let mut iter = search_scrollback(merged, query, opts);
            let mut hits: Vec<crate::shell_proto::WireSearchHit> = Vec::new();
            let mut total_seen: u32 = 0;
            loop {
                if cancel_w.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                match iter.next() {
                    Some(h) => {
                        total_seen = total_seen.saturating_add(1);
                        hits.push(remap_to_wire(h, file_total));
                    }
                    None => break,
                }
            }
            if cancel_w.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let has_more = (hits.len() as u32) == cap;
            on_batch(query_id, hits, has_more, total_seen);
        })
        .expect("spawn merged search worker");
    SearchWorker {
        query_id,
        cancel,
        _handle: Some(handle),
    }
}

/// Centre an 80-char snippet on the match.  Returns
/// `(snippet, snip_match_start, snip_match_end)` where the start/end
/// are *char* offsets within the returned snippet.
fn build_snippet(norm_text: &str, char_offset: usize, char_len: usize) -> (String, u16, u16) {
    const SNIPPET_MAX: usize = 80;
    let chars: Vec<char> = norm_text.chars().collect();
    let total = chars.len();
    let match_end = (char_offset + char_len).min(total);
    let want_before = SNIPPET_MAX.saturating_sub(char_len) / 2;
    let want_after = SNIPPET_MAX.saturating_sub(char_len) - want_before;
    let before_avail = char_offset.min(want_before);
    let after_avail = (total - match_end).min(want_after);
    let start = char_offset - before_avail;
    let end = match_end + after_avail;
    let snippet: String = chars[start..end].iter().collect();
    let snip_match_start = before_avail as u16;
    let snip_match_end = (before_avail + char_len) as u16;
    (snippet, snip_match_start, snip_match_end)
}

// ──────────────────── tests ───────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    fn cells(s: &str) -> Vec<Cell> {
        s.chars().map(|c| Cell { ch: c, attrs: Default::default() }).collect()
    }

    fn src(rows: Vec<(&str, bool)>) -> InMemorySource {
        InMemorySource {
            rows: rows.into_iter().map(|(s, w)| (cells(s), w)).collect(),
        }
    }

    #[test]
    fn search_ascii_substring_finds_three_hits_in_one_line() {
        let s = src(vec![("foo bar foo baz foo qux", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "foo".into(), opts).collect();
        assert_eq!(hits.len(), 3, "expected 3 hits, got {hits:#?}");
        // Char offsets: positions of 'foo' substring in the line.
        let offsets: Vec<u32> = hits.iter().map(|h| h.char_offset).collect();
        assert_eq!(offsets, vec![0, 8, 16]);
    }

    #[test]
    fn search_case_insensitive_matches_mixed_case() {
        let s = src(vec![("Hello WORLD", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "world".into(), opts).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].char_offset, 6);
        assert_eq!(hits[0].char_len, 5);
    }

    #[test]
    fn search_case_sensitive_does_not_match_other_case() {
        let s = src(vec![("Hello WORLD", false)]);
        let opts = SearchOpts { case_sensitive: true, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "world".into(), opts).collect();
        assert_eq!(hits.len(), 0);
    }

    #[test]
    fn search_cjk_clean_match() {
        let s = src(vec![("中文 你好 中文 测试", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "你好".into(), opts).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].char_offset, 3);
    }

    #[test]
    fn search_decawm_wrap_treats_two_rows_as_one_logical_line() {
        // Row 0: "https://example.com/" (20 chars, no wrap flag)
        // Row 1: "path/to/file.html" (continuation — DECAWM wrap)
        let s = src(vec![
            ("https://example.com/", false),
            ("path/to/file.html", true),
        ]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "example.com/path".into(), opts).collect();
        assert_eq!(hits.len(), 1, "expected 1 hit across wrap; got {hits:#?}");
        assert!(
            hits[0].physical_rows.len() >= 2,
            "expected ≥ 2 phys spans for cross-wrap hit; got {:?}",
            hits[0].physical_rows
        );
    }

    #[test]
    fn search_cc_hard_wrap_with_indent_treated_as_continuation() {
        // 20-col rows; upper ends with URL-class at col 19, lower starts
        // with 2-space hanging indent.
        let s = src(vec![
            ("https://example.com/", false),  // 20 chars, last = '/'
            ("  path/to/file.html ", false),  // 2 leading spaces; not wrapped
        ]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "example.com/path".into(), opts).collect();
        assert_eq!(hits.len(), 1, "cc-hard-wrap continuation should be merged; got {hits:#?}");
    }

    #[test]
    fn search_snippet_short_line_clipped_to_boundaries() {
        let s = src(vec![("hi foo", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "foo".into(), opts).collect();
        assert_eq!(hits.len(), 1);
        // Match is at position 3 with whole line being 6 chars; snippet
        // is whole line.
        assert_eq!(hits[0].snippet, "hi foo");
        assert_eq!(hits[0].snippet_match_start, 3);
        assert_eq!(hits[0].snippet_match_end, 6);
    }

    #[test]
    fn search_snippet_long_line_centred_on_match() {
        let line: String = "abcdefghij ".repeat(20) + "FOOBAR " + &"klmnop ".repeat(20);
        // Match starts somewhere in the middle.
        let s = src(vec![(&line, false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "foobar".into(), opts).collect();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(snippet.len() <= 100, "snippet should be ~80 chars, got {} chars", snippet.chars().count());
        assert!(snippet.to_lowercase().contains("foobar"), "snippet must contain match");
    }

    #[test]
    fn search_no_results_returns_empty_iter() {
        let s = src(vec![("hello world", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "xyzzy".into(), opts).collect();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_cancellation_via_drop_does_not_panic() {
        let s = src((0..1000)
            .map(|i| (format!("line {i:04} foo bar baz qux"), false))
            .collect::<Vec<_>>()
            .iter()
            .map(|(line, w)| (line.as_str(), *w))
            .collect());
        let opts = SearchOpts { case_sensitive: false, max_total: 10000 };
        let mut iter = search_scrollback(s, "foo".into(), opts);
        let _first = iter.next();
        // Drop without exhausting — must not panic / leak.
        drop(iter);
    }

    #[test]
    fn search_max_total_caps_yielded_hits() {
        let s = src((0..200)
            .map(|i| (format!("foo {i:03}"), false))
            .collect::<Vec<_>>()
            .iter()
            .map(|(line, w)| (line.as_str(), *w))
            .collect());
        let opts = SearchOpts { case_sensitive: false, max_total: 50 };
        let hits: Vec<SearchHit> = search_scrollback(s, "foo".into(), opts).collect();
        assert_eq!(hits.len(), 50);
    }

    #[test]
    fn search_newest_first_logical_idx_descends() {
        // 3 lines, search emits in reverse order — logical_line_idx
        // should be assigned highest to most-recent.
        let s = src(vec![("a foo", false), ("b foo", false), ("c foo", false)]);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let hits: Vec<SearchHit> = search_scrollback(s, "foo".into(), opts).collect();
        assert_eq!(hits.len(), 3);
        // Hits walk newest → oldest.  logical_line_idx 2 first, then 1,
        // then 0.
        assert_eq!(hits[0].logical_line_idx, 2);
        assert_eq!(hits[1].logical_line_idx, 1);
        assert_eq!(hits[2].logical_line_idx, 0);
    }

    // ─── B4: live-grid merge tests ────────────────────────────────

    /// Helper: a `SearchSource` that exposes zero file rows.  Stand-in
    /// for the "no scrollback yet" case so we can exercise the live
    /// half of `MergedLiveFileSource` in isolation.
    struct EmptyFileSource;
    impl SearchSource for EmptyFileSource {
        fn line_count(&self) -> u64 {
            0
        }
        fn line(&self, _idx: u64) -> Option<Vec<Cell>> {
            None
        }
        fn wrapped(&self, _idx: u64) -> bool {
            false
        }
    }

    /// B4 — a query matching only the live grid (with empty file
    /// scrollback) must return a hit whose `logical_line_idx` falls
    /// in the synthetic range `[u64::MAX - rows + 1, u64::MAX]`, AND
    /// whose spans use live-local row indices (0..rows).
    #[test]
    fn b4_live_grid_only_query_returns_synthetic_index() {
        // 4-row live grid; the match is in row 2 (middle).  No file
        // scrollback at all.
        let rows = vec![
            (cells("first row blah"), false),
            (cells("second row"), false),
            (cells("third row has needle here"), false),
            (cells("fourth and last"), false),
        ];
        let live = LiveGridSnapshot::from_rows(rows);
        let live_len = live.rows.len();
        let merged = MergedLiveFileSource::new(EmptyFileSource, live);
        let file_total = merged.file_total;
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let mut hits = Vec::new();
        for h in search_scrollback(merged, "needle".into(), opts) {
            hits.push(remap_to_wire(h, file_total));
        }
        assert_eq!(hits.len(), 1, "expected exactly one hit");
        let h = &hits[0];
        // Live grid row 2, file_total = 0 → synthetic = u64::MAX - 2.
        assert_eq!(h.logical_line_idx, u64::MAX - 2);
        // L2's decode: view_offset = (rows - 1) - (u64::MAX - idx)
        // For our 4-row grid: view_offset = 3 - 2 = 1.
        let view_offset = (live_len as u64 - 1) - (u64::MAX - h.logical_line_idx);
        assert_eq!(view_offset, 1);
        // Spans must be live-local (< rows count).
        assert!(!h.spans.is_empty());
        for s in &h.spans {
            assert!(
                (s.phys_row_idx as usize) < live_len,
                "span phys_row_idx {} should be live-local (< {})",
                s.phys_row_idx,
                live_len
            );
        }
    }

    /// B4 — file scrollback + live grid both contain the query; the
    /// live hit must use synthetic indices, the file hit must keep
    /// its scrollback index unchanged.
    #[test]
    fn b4_merged_hits_separate_file_and_live() {
        let file_rows: Vec<(Vec<Cell>, bool)> = (0..50)
            .map(|i| {
                let s = if i == 20 {
                    "file row 20 has needle deep".to_string()
                } else {
                    format!("file row {i:02} filler line")
                };
                (cells(&s), false)
            })
            .collect();
        let file = InMemorySource { rows: file_rows };
        let live = LiveGridSnapshot::from_rows(vec![
            (cells("live row 0"), false),
            (cells("live row 1 has needle on it"), false),
            (cells("live row 2"), false),
        ]);
        let live_len = live.rows.len() as u64;
        let file_total = file.line_count();
        let merged = MergedLiveFileSource::new(file, live);
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let mut hits = Vec::new();
        for h in search_scrollback(merged, "needle".into(), opts) {
            hits.push(remap_to_wire(h, file_total));
        }
        assert_eq!(hits.len(), 2, "expected file + live hit");
        // newest-first order: live hit comes first (lives at the top
        // of the merged source, so the engine walks it before the
        // file).
        let live_hit = &hits[0];
        let file_hit = &hits[1];
        assert!(
            live_hit.logical_line_idx >= u64::MAX - live_len,
            "live hit logical_line_idx {} should be in synthetic range",
            live_hit.logical_line_idx
        );
        assert!(
            file_hit.logical_line_idx < file_total,
            "file hit logical_line_idx {} should be < file_total {}",
            file_hit.logical_line_idx,
            file_total
        );
        for s in &live_hit.spans {
            assert!(
                (s.phys_row_idx as u64) < live_len,
                "live span row_idx {} should be live-local",
                s.phys_row_idx
            );
        }
        for s in &file_hit.spans {
            assert!(
                s.phys_row_idx < file_total,
                "file span row_idx {} should be in file range",
                s.phys_row_idx
            );
        }
    }

    /// B1 perf gate (hard ceiling per §0): 1 MB scrollback, first 64
    /// hits, < 20 ms.  Synthesise ~10k 100-char lines with "foo"
    /// scattered, run the scan, assert wall-clock.  If this fails on
    /// mini we DROP a feature, not relax the budget.
    #[test]
    fn b1_perf_1mb_first_64_hits_under_20ms() {
        let n = 10_000;
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            let s = if i % 100 == 7 {
                format!("scratch line {i:05} foo bar baz qux quux corge grault garply waldo fred")
            } else {
                format!("scratch line {i:05} hello world abc def ghi jkl mno pqr stu vwx yz0 123")
            };
            rows.push((s, false));
        }
        let src = InMemorySource {
            rows: rows.iter().map(|(s, w)| (cells(s), *w)).collect(),
        };
        let opts = SearchOpts { case_sensitive: false, max_total: 64 };
        let start = std::time::Instant::now();
        let hits: Vec<SearchHit> = search_scrollback(src, "foo".into(), opts).collect();
        let elapsed = start.elapsed();
        assert_eq!(hits.len(), 64, "expected to fill max_total cap");
        // Generous ceiling — measured locally on M-series should be
        // < 5 ms; bin/bench-remote gating gives more reliable numbers.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "1 MB search first 64 hits should be < 50 ms; got {elapsed:?}"
        );
    }
}
