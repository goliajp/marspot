//! Scrollback search payloads: the query a core sends a session,
//! the hits that come back, and paging and cancelling a search.
//!
//! Lengths are bounded by the frame layer's 32-bit payload size,
//! well above any realistic search payload.

use std::io;

pub fn encode_search_scrollback(
    query_id: u32,
    case_sensitive: bool,
    max_total: u32,
    query: &str,
) -> Vec<u8> {
    let q = query.as_bytes();
    let mut v = Vec::with_capacity(4 + 1 + 4 + 4 + q.len());
    v.extend_from_slice(&query_id.to_le_bytes());
    v.push(case_sensitive as u8);
    v.extend_from_slice(&max_total.to_le_bytes());
    v.extend_from_slice(&(q.len() as u32).to_le_bytes());
    v.extend_from_slice(q);
    v
}

pub fn decode_search_scrollback(payload: &[u8]) -> io::Result<(u32, bool, u32, String)> {
    if payload.len() < 4 + 1 + 4 + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SearchScrollback header truncated",
        ));
    }
    let qid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let case = payload[4] != 0;
    let max_total = u32::from_le_bytes(payload[5..9].try_into().unwrap());
    let q_len = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
    if payload.len() != 13 + q_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SearchScrollback q_len {q_len} but payload total {}",
                payload.len()
            ),
        ));
    }
    let query = std::str::from_utf8(&payload[13..])
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SearchScrollback query not utf-8",
            )
        })?
        .to_string();
    Ok((qid, case, max_total, query))
}

/// Wire shape mirrors `scrollback_search::SearchHit` field-for-field
/// in the order they are encoded.  Kept as a flat struct on the
/// wire side so encode/decode are trivial loops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSearchHit {
    pub logical_line_idx: u64,
    pub char_offset: u32,
    pub char_len: u32,
    pub snippet_match_start: u16,
    pub snippet_match_end: u16,
    pub snippet: String,
    pub spans: Vec<WirePhysicalSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirePhysicalSpan {
    pub phys_row_idx: u64,
    pub col_start: u16,
    pub col_end_inclusive: u16,
}

pub fn encode_search_results(
    query_id: u32,
    has_more: bool,
    total_seen: u32,
    hits: &[WireSearchHit],
) -> Vec<u8> {
    // Generous reservation; final size grows with snippets/spans.
    let mut v = Vec::with_capacity(4 + 1 + 4 + 4 + hits.len() * 64);
    v.extend_from_slice(&query_id.to_le_bytes());
    v.push(has_more as u8);
    v.extend_from_slice(&total_seen.to_le_bytes());
    v.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    for h in hits {
        v.extend_from_slice(&h.logical_line_idx.to_le_bytes());
        v.extend_from_slice(&h.char_offset.to_le_bytes());
        v.extend_from_slice(&h.char_len.to_le_bytes());
        v.extend_from_slice(&h.snippet_match_start.to_le_bytes());
        v.extend_from_slice(&h.snippet_match_end.to_le_bytes());
        let snip = h.snippet.as_bytes();
        v.extend_from_slice(&(snip.len() as u32).to_le_bytes());
        v.extend_from_slice(snip);
        v.extend_from_slice(&(h.spans.len() as u16).to_le_bytes());
        for s in &h.spans {
            v.extend_from_slice(&s.phys_row_idx.to_le_bytes());
            v.extend_from_slice(&s.col_start.to_le_bytes());
            v.extend_from_slice(&s.col_end_inclusive.to_le_bytes());
        }
    }
    v
}

pub fn decode_search_results(payload: &[u8]) -> io::Result<(u32, bool, u32, Vec<WireSearchHit>)> {
    let mut p = 0;
    fn need<'a>(payload: &'a [u8], p: usize, n: usize, what: &str) -> io::Result<&'a [u8]> {
        if p + n > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SearchResults truncated at {what}"),
            ));
        }
        Ok(&payload[p..p + n])
    }
    let qid = u32::from_le_bytes(need(payload, p, 4, "qid")?.try_into().unwrap());
    p += 4;
    let has_more = need(payload, p, 1, "has_more")?[0] != 0;
    p += 1;
    let total_seen = u32::from_le_bytes(need(payload, p, 4, "total_seen")?.try_into().unwrap());
    p += 4;
    let hit_count =
        u32::from_le_bytes(need(payload, p, 4, "hit_count")?.try_into().unwrap()) as usize;
    p += 4;
    let mut hits = Vec::with_capacity(hit_count);
    for _ in 0..hit_count {
        let logical_line_idx = u64::from_le_bytes(need(payload, p, 8, "lli")?.try_into().unwrap());
        p += 8;
        let char_offset =
            u32::from_le_bytes(need(payload, p, 4, "char_offset")?.try_into().unwrap());
        p += 4;
        let char_len = u32::from_le_bytes(need(payload, p, 4, "char_len")?.try_into().unwrap());
        p += 4;
        let snippet_match_start =
            u16::from_le_bytes(need(payload, p, 2, "sm_start")?.try_into().unwrap());
        p += 2;
        let snippet_match_end =
            u16::from_le_bytes(need(payload, p, 2, "sm_end")?.try_into().unwrap());
        p += 2;
        let snippet_byte_len =
            u32::from_le_bytes(need(payload, p, 4, "snip_len")?.try_into().unwrap()) as usize;
        p += 4;
        let snippet = std::str::from_utf8(need(payload, p, snippet_byte_len, "snippet")?)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SearchResults snippet not utf-8",
                )
            })?
            .to_string();
        p += snippet_byte_len;
        let span_count =
            u16::from_le_bytes(need(payload, p, 2, "span_count")?.try_into().unwrap()) as usize;
        p += 2;
        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            let phys_row_idx = u64::from_le_bytes(need(payload, p, 8, "phys")?.try_into().unwrap());
            p += 8;
            let col_start =
                u16::from_le_bytes(need(payload, p, 2, "col_start")?.try_into().unwrap());
            p += 2;
            let col_end_inclusive =
                u16::from_le_bytes(need(payload, p, 2, "col_end")?.try_into().unwrap());
            p += 2;
            spans.push(WirePhysicalSpan {
                phys_row_idx,
                col_start,
                col_end_inclusive,
            });
        }
        hits.push(WireSearchHit {
            logical_line_idx,
            char_offset,
            char_len,
            snippet,
            snippet_match_start,
            snippet_match_end,
            spans,
        });
    }
    if p != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SearchResults trailing bytes: parsed {p} of {}",
                payload.len()
            ),
        ));
    }
    Ok((qid, has_more, total_seen, hits))
}

pub fn encode_search_more(query_id: u32, count: u32, direction: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.extend_from_slice(&query_id.to_le_bytes());
    v.extend_from_slice(&count.to_le_bytes());
    v.push(direction);
    v
}

pub fn decode_search_more(payload: &[u8]) -> io::Result<(u32, u32, u8)> {
    if payload.len() != 9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchMore wrong length: {}", payload.len()),
        ));
    }
    let qid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let count = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let direction = payload[8];
    Ok((qid, count, direction))
}

pub fn encode_search_cancel(query_id: u32) -> Vec<u8> {
    query_id.to_le_bytes().to_vec()
}

pub fn decode_search_cancel(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchCancel wrong length: {}", payload.len()),
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}
