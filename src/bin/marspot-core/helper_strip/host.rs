//! A pane with helpers: how many rows its strip has, and what is
//! listed in it.

use super::compose::{Arrow, Drawn, Row, State};
use super::height::{rows_for, Height, Merge, Said};
use super::native_list::{self, Columns};
use super::theme::Theme;
use marspot::shell_proto::{HelperStrip, StripAgent, StripRow, StripState, HELPER_STRIP_ROWS};
use marspot_term::grid::Grid;

/// A helper that is listed.
#[derive(Default)]
struct Listed {
    sid: u64,
    /// What it is called in the list.
    label: String,
    /// Its session was seen ready, or it had ended when first heard of.
    ready: bool,
}

#[derive(Default)]
pub(super) struct Host {
    pub height: Height,
    batch: u64,
    /// One of the batch was seen at work.
    alive: bool,
    /// Those of the record, in its order.
    listed: Vec<Listed>,
    /// Those of the batch the user cleared.
    hidden: Vec<u64>,
    look: Look,
    /// Whether the agent's own list is there, as the height takes it.
    pub merge: Merge,
    pub drawn: Drawn,
    /// One is at work and its figures change by the second.
    pub ticks: bool,
    /// The size its helpers are to have, and since when.
    pub block: Option<((u16, u16), std::time::Instant)>,
    /// The cells of its pane when it was last laid out, with the strip's.
    pub cells: Option<(u16, u16)>,
    /// The strip last written to the log (dev seam).
    pub dumped: u64,
}

/// What an agent's screen says of how to draw.
#[derive(Default)]
struct Look {
    /// The session and the frame of it this is of.
    of: Option<(u64, u64)>,
    theme: Option<&'static Theme>,
    native: Option<Columns>,
}

fn state(r: &StripRow) -> State {
    match r.state {
        StripState::Preparing | StripState::Working => State::Running,
        StripState::Waiting => State::Awaiting,
        StripState::Question => State::Asked,
        StripState::Idle => State::Idle,
        StripState::Exited => State::Done,
        StripState::Stopped => State::Stopped,
        StripState::Unknown(_) if r.running_since_ms.is_some() => State::Running,
        StripState::Unknown(_) => State::Done,
    }
}

impl Host {
    pub(super) fn found(rows: u16) -> Self {
        Self { height: Height::found(rows), ..Self::default() }
    }

    /// Take in a record that may differ from the last.
    pub(super) fn heard(&mut self, record: &HelperStrip) {
        if record.batch != self.batch {
            self.batch = record.batch;
            self.alive = false;
            self.hidden.clear();
        }
        self.alive |= record.rows.iter().any(|r| r.running_since_ms.is_some());
        let old = std::mem::take(&mut self.listed);
        let twice = |r: &StripRow| record.rows.iter().filter(|o| o.name == r.name).count() > 1;
        self.listed = record
            .rows
            .iter()
            .map(|r| Listed {
                sid: r.sid,
                label: match r.number {
                    Some(n) if twice(r) => format!("{} {n:04}", r.name),
                    _ => r.name.clone(),
                },
                ready: old.iter().find(|o| o.sid == r.sid).map_or(r.gone, |o| o.ready),
            })
            .collect();
    }

    /// Clear a row: it is listed no more in this batch.
    pub(super) fn hide(&mut self, sid: u64) {
        if !self.hidden.contains(&sid) && self.hidden.len() < HELPER_STRIP_ROWS {
            self.hidden.push(sid);
        }
    }

    pub(super) fn shows(&self, sid: u64) -> bool {
        !self.hidden.contains(&sid)
    }

    /// Note which helpers' sessions are ready.
    pub(super) fn see(&mut self, ready: impl Fn(u64) -> bool) {
        for l in self.listed.iter_mut().filter(|l| !l.ready) {
            l.ready = ready(l.sid);
        }
    }

    /// What `record` comes to; `told`: that there is none means there
    /// are no helpers; `inline`: the agent draws on its main screen.
    pub(super) fn said(&self, record: Option<&HelperStrip>, told: bool, inline: bool) -> Said {
        let Some(record) = record else {
            return if told { Said::Nothing } else { Said::Unknown };
        };
        let mut rows = record.rows.iter().filter(|r| self.shows(r.sid)).peekable();
        if record.is_empty() {
            return Said::Nothing;
        }
        if rows.peek().is_none() {
            return Said::Cleared;
        }
        let works = rows.any(|r| r.running_since_ms.is_some());
        let ready = self.listed.iter().filter(|l| l.ready && self.shows(l.sid)).count();
        Said::Rows { works, need: if self.alive { rows_for(ready, self.merge.merged(), inline) } else { 0 } }
    }

    /// The rows to list at `now_ms`, with the panes they are of.
    pub(super) fn rows<'a>(&'a self, record: &'a HelperStrip, now_ms: u64, out: &mut [Row<'a>; HELPER_STRIP_ROWS]) -> usize {
        let mut n = 0;
        for (r, l) in record.rows.iter().zip(&self.listed).filter(|(r, _)| self.shows(r.sid)) {
            out[n] = Row {
                name: &l.label,
                named: false,
                task: &r.task,
                state: state(r),
                elapsed_ms: r.elapsed_ms(now_ms),
                tokens: r.tokens,
                arrow: if r.had_tool { Arrow::Down } else { Arrow::Up },
            };
            n += 1;
            if n == out.len() {
                break;
            }
        }
        n
    }

    /// The helper listed `i`th of those `rows` lists.
    pub(super) fn sid_at(&self, record: &HelperStrip, i: usize) -> Option<u64> {
        record.rows.iter().filter(|r| self.shows(r.sid)).nth(i).map(|r| r.sid)
    }

    /// How the helper listed `i`th is.
    pub(super) fn state_at(&self, record: &HelperStrip, i: usize) -> Option<State> {
        record.rows.iter().filter(|r| self.shows(r.sid)).nth(i).map(state)
    }

    /// How many rows `record` lists.
    pub(super) fn shown_rows(&self, record: &HelperStrip) -> usize {
        record.rows.iter().filter(|r| self.shows(r.sid)).take(HELPER_STRIP_ROWS).count()
    }

    /// Where helper `sid` is listed of those `rows` lists.
    pub(super) fn index_of(&self, record: &HelperStrip, sid: u64) -> Option<usize> {
        record.rows.iter().filter(|r| self.shows(r.sid)).take(HELPER_STRIP_ROWS).position(|r| r.sid == sid)
    }

    /// Whether what `record` lists changes by the second.
    pub(super) fn ticking(&self, record: &HelperStrip) -> bool {
        record.rows.iter().any(|r| self.shows(r.sid) && r.running_since_ms.is_some() && state(r) == State::Running)
    }

    /// Look at the screen of the pane: whether the agent's own list is
    /// on it.  Once per frame of it, as for drawing.
    pub(super) fn glance(&mut self, agent: StripAgent, screen: &Grid, of: (u64, u64), scrolled: bool, now: std::time::Instant) {
        let native = self.look(agent, screen, of, scrolled).1;
        if !scrolled {
            self.merge.saw(native.is_some(), now);
        }
        self.merge.settle(now);
    }

    /// The theme to draw in and the columns of the agent's own list,
    /// read from its screen once per frame of it.  A screen that shows
    /// its history says neither.
    pub(super) fn look(&mut self, agent: StripAgent, screen: &Grid, of: (u64, u64), scrolled: bool) -> (&'static Theme, Option<Columns>) {
        if self.look.of != Some(of) && !scrolled {
            self.look.of = Some(of);
            let claude = agent == StripAgent::Claude;
            self.look.native = claude.then(|| native_list::find(screen)).flatten().and_then(|n| n.columns);
            if claude && let Some(t) = Theme::of_screen(screen) {
                self.look.theme = Some(t);
            }
        }
        let light = marspot_term::palette::current_id() == marspot_term::palette::ThemeId::Light;
        (self.look.theme.unwrap_or_else(|| Theme::for_paper(light)), self.look.native.filter(|_| !scrolled))
    }
}
