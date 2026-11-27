//! `HelperStrip`: the helpers a pane's strip lists.
//!
//! The whole record each time.  A pane nothing was said of and a pane
//! with an empty record differ: the first is not known yet.
//!
//! Once every record there is has gone to a new core, an empty one of
//! pane 0 follows: a pane nothing was said of by then has none.
//!
//! Layout: root sid u64, batch u64, parent agent u8, flags u8, row count
//! u8, then per row a u16 byte length and that many bytes, so a later
//! build can lengthen a row: sid u64, number u16, state u8, agent u8,
//! flags u8, started_ms u64, worked_ms u64, running_since_ms u64,
//! ended_ms u64, tokens u64, then name, model and task as text.

use std::io;

use super::wire::{Rd, finish, put_text};

/// Rows a record carries at most: as many as there may be shadow panes.
pub const HELPER_STRIP_ROWS: usize = 24;
/// Bytes of a row's name and of its model at most.
pub const HELPER_STRIP_NAME_MAX: usize = 64;
/// Bytes of a row's task at most: 120 characters of any kind.
pub const HELPER_STRIP_TASK_MAX: usize = 480;

const TOKENS_UNKNOWN: u64 = u64::MAX;
const ROW_GONE: u8 = 1;
const ROW_HAD_TOOL: u8 = 1 << 1;
// 1 << 2 is set by older shells and read by no one
const ROW_PLAIN: u8 = 1 << 3;

wire_enum!(
    /// What a row's helper is doing.  `Waiting`: at an approval prompt;
    /// `Question`: at a question it asked, waiting for the answer.
    StripState { Preparing = 0, Working = 1, Idle = 2, Waiting = 3, Exited = 4, Stopped = 5, Question = 6 }
);
wire_enum!(
    /// `Other`: no agent, or none of these.
    StripAgent { Other = 0, Claude = 1, Codex = 2, Gemini = 3, Kimi = 4 }
);

/// L1 → L2: the helpers under `root_sid`, a pane that is no shadow, in
/// the order they started.  No rows: it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperStrip {
    pub root_sid: u64,
    /// Which batch the rows are; a later one has a larger number, over
    /// a restart of the shell too.  0 with no rows.
    pub batch: u64,
    /// The agent in front in the root pane.
    pub parent_agent: StripAgent,
    /// No bit is defined yet.
    pub flags: u8,
    /// The first `HELPER_STRIP_ROWS` of them go.
    pub rows: Vec<StripRow>,
}

impl HelperStrip {
    pub fn empty(root_sid: u64) -> Self {
        Self { root_sid, batch: 0, parent_agent: StripAgent::Other, flags: 0, rows: Vec::new() }
    }

    /// That every record there is was sent.
    pub fn all_told() -> Self {
        Self::empty(0)
    }

    pub fn is_all_told(&self) -> bool {
        self.root_sid == 0
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StripRow {
    pub sid: u64,
    /// The pane's four-digit code, to tell rows of one name apart.
    pub number: Option<u16>,
    pub state: StripState,
    pub agent: StripAgent,
    /// Its session is over: there is nothing to look at.
    pub gone: bool,
    /// A tool was called, or it is not known that none was.
    pub had_tool: bool,
    /// A shadow pane no helper runs in: what is in front of it, with no
    /// figures, and it keeps the size it was opened with.
    pub plain: bool,
    /// Ms after the epoch.
    pub started_ms: u64,
    /// How long it was at work before `running_since_ms`.
    pub worked_ms: u64,
    /// Since when it is at work (ms after the epoch); None: it is not.
    pub running_since_ms: Option<u64>,
    /// When it stopped (ms after the epoch); None while it is at work.
    /// One that is idle has one too, until it works again.
    pub ended_ms: Option<u64>,
    /// The size of its context at the last response.
    pub tokens: Option<u64>,
    pub name: String,
    /// Empty: its agent's default.
    pub model: String,
    /// The task's first line.
    pub task: String,
}

impl StripRow {
    /// How long it has been at work by `now_ms`.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.worked_ms.saturating_add(self.running_since_ms.map_or(0, |since| now_ms.saturating_sub(since)))
    }

    fn flags(&self) -> u8 {
        (self.gone as u8 * ROW_GONE) | (self.had_tool as u8 * ROW_HAD_TOOL) | (self.plain as u8 * ROW_PLAIN)
    }
}

/// The start of `s`, `max` bytes at most, cut between characters.
fn cut(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

fn encode_row(r: &StripRow) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(59 + r.name.len() + r.model.len() + r.task.len());
    v.extend_from_slice(&r.sid.to_le_bytes());
    v.extend_from_slice(&r.number.unwrap_or(0).to_le_bytes());
    v.push(r.state.to_u8());
    v.push(r.agent.to_u8());
    v.push(r.flags());
    v.extend_from_slice(&r.started_ms.to_le_bytes());
    v.extend_from_slice(&r.worked_ms.to_le_bytes());
    v.extend_from_slice(&r.running_since_ms.unwrap_or(0).to_le_bytes());
    v.extend_from_slice(&r.ended_ms.unwrap_or(0).to_le_bytes());
    v.extend_from_slice(&r.tokens.unwrap_or(TOKENS_UNKNOWN).to_le_bytes());
    put_text(&mut v, cut(&r.name, HELPER_STRIP_NAME_MAX), "helper_strip")?;
    put_text(&mut v, cut(&r.model, HELPER_STRIP_NAME_MAX), "helper_strip")?;
    put_text(&mut v, cut(&r.task, HELPER_STRIP_TASK_MAX), "helper_strip")?;
    Ok(v)
}

fn decode_row(p: &[u8]) -> io::Result<StripRow> {
    let mut r = Rd::new(p, "helper_strip row");
    let (sid, number) = (r.u64()?, r.u16()?);
    let (state, agent, flags) = (r.u8()?, r.u8()?, r.u8()?);
    Ok(StripRow {
        sid,
        number: Some(number).filter(|n| *n != 0),
        state: StripState::from_u8(state),
        agent: StripAgent::from_u8(agent),
        gone: flags & ROW_GONE != 0,
        had_tool: flags & ROW_HAD_TOOL != 0,
        plain: flags & ROW_PLAIN != 0,
        started_ms: r.u64()?,
        worked_ms: r.u64()?,
        running_since_ms: Some(r.u64()?).filter(|t| *t != 0),
        ended_ms: Some(r.u64()?).filter(|t| *t != 0),
        tokens: Some(r.u64()?).filter(|t| *t != TOKENS_UNKNOWN),
        name: cut(&r.text()?, HELPER_STRIP_NAME_MAX).to_owned(),
        model: cut(&r.text()?, HELPER_STRIP_NAME_MAX).to_owned(),
        task: cut(&r.text()?, HELPER_STRIP_TASK_MAX).to_owned(),
    })
}

pub fn encode_helper_strip(m: &HelperStrip) -> io::Result<Vec<u8>> {
    let rows = &m.rows[..m.rows.len().min(HELPER_STRIP_ROWS)];
    let mut v = Vec::with_capacity(19 + 160 * rows.len());
    v.extend_from_slice(&m.root_sid.to_le_bytes());
    v.extend_from_slice(&m.batch.to_le_bytes());
    v.push(m.parent_agent.to_u8());
    v.push(m.flags);
    v.push(rows.len() as u8);
    for r in rows {
        let row = encode_row(r)?;
        v.extend_from_slice(&(row.len() as u16).to_le_bytes());
        v.extend_from_slice(&row);
    }
    finish(v, "helper_strip")
}

pub fn decode_helper_strip(p: &[u8]) -> io::Result<HelperStrip> {
    let mut r = Rd::new(p, "helper_strip");
    let (root_sid, batch) = (r.u64()?, r.u64()?);
    let parent_agent = StripAgent::from_u8(r.u8()?);
    let flags = r.u8()?;
    let n = (r.u8()? as usize).min(HELPER_STRIP_ROWS);
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        let len = r.u16()? as usize;
        rows.push(decode_row(r.take(len)?)?);
    }
    Ok(HelperStrip { root_sid, batch, parent_agent, flags, rows })
}

/// `StripKeys`, L2 → L1: whether window `window_id`'s keys are a
/// strip's.  While they are, a plain `x` is dispatched without the input
/// method, which would compose it.  L1 → L2: the same frame back, which
/// says the shell does that; a core told nothing offers no `x`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripKeys {
    pub window_id: u32,
    pub on: bool,
}

pub fn encode_strip_keys(m: &StripKeys) -> Vec<u8> {
    let mut v = m.window_id.to_le_bytes().to_vec();
    v.push(m.on as u8);
    v
}

pub fn decode_strip_keys(p: &[u8]) -> io::Result<StripKeys> {
    let mut r = Rd::new(p, "strip_keys");
    Ok(StripKeys { window_id: r.u32()?, on: r.u8()? != 0 })
}
