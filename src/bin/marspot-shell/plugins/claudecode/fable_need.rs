//! Sessions that asked for Fable and were refused it.
//!
//! A pane is judged by the model it works in, and an Opus pane can
//! still need Fable: a subagent or a skill that names it, or a switch
//! of model, sends a request that the account answers from Fable's own
//! cap. Nothing about the pane says so in advance; the refusal does.
//! Rebalance had been moving such Opus panes onto accounts whose Fable
//! was spent, to leave Fable to the panes that need it, and on
//! 2026-11-22 a session on P6 had a background agent refused with
//! "You've hit your Fable limit" and its work stopped there.
//!
//! Once refused, the session is judged as a Fable pane: it is moved at
//! once to an account with Fable left, told what to start again, and
//! from then on goes only where Fable is left and is never moved off
//! such an account to save Fable for others.

use std::collections::HashMap;
use std::time::Duration;

use super::quota::{needs_fable, Refusal, Room};
use super::{BindMeta, ClaudecodePlugin, ScanResult};
use crate::plugins::{LogLevel, PluginHost};

/// How long one refusal marks a pane. A session that asked for Fable
/// once tends to ask again; the bound keeps a pane whose conversation
/// has since moved on from being held to Fable accounts for good.
pub(super) const MARK_FOR: Duration = Duration::from_secs(12 * 3600);

/// The model a marked pane is judged as, as the feed labels it.
pub(super) const FABLE: &str = "fable";

/// A request for Fable that the API refused under the claude now
/// running in a pane.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) struct Ask {
    /// Unix seconds, from the record's own timestamp.
    pub at: i64,
    /// The background agent that was refused, by the description its
    /// notice gives (empty when it gives none); `None` when it was the
    /// session's own request.
    pub agent: Option<String>,
}

/// The newer of the two refusals, when it is about Fable.
///
/// `agent_name` is only asked for when the agent's refusal is the one.
pub(super) fn ask_of(own: Option<&Refusal>, agent: Option<&Refusal>, agent_name: impl FnOnce() -> String) -> Option<Ask> {
    let fable = |r: &&Refusal| needs_fable(Some(&r.model));
    match (own.filter(fable), agent.filter(fable)) {
        (Some(o), Some(a)) if o.at >= a.at => Some(Ask { at: o.at, agent: None }),
        (_, Some(a)) => Some(Ask { at: a.at, agent: Some(agent_name()) }),
        (Some(o), None) => Some(Ask { at: o.at, agent: None }),
        (None, None) => None,
    }
}

/// The panes marked as needing Fable. Bounded by the panes.
///
/// Persisted (`fable_need.tsv`): an L1 self-update starts a new
/// process, and a mark lost there sends the pane back to an account
/// without Fable on the next rebalance.
#[derive(Debug, Default)]
pub(super) struct Marks {
    by_pane: HashMap<u64, Mark>,
    dirty: bool,
}

#[derive(Debug, PartialEq)]
struct Mark {
    ask: Ask,
    /// Unix seconds when the pane was marked.
    since: i64,
    /// The session has been told about this refusal, with the move
    /// that answered it.
    told: bool,
}

impl Marks {
    /// Take in a refusal seen in `sid` at `now`; true when it is newer
    /// than the one the pane is already marked for.
    pub(super) fn note(&mut self, sid: u64, ask: &Ask, now: i64) -> bool {
        if self.by_pane.get(&sid).is_some_and(|m| m.ask.at >= ask.at) {
            return false;
        }
        self.by_pane.insert(sid, Mark { ask: ask.clone(), since: now, told: false });
        self.dirty = true;
        true
    }

    /// Forget panes that are gone, and marks older than `MARK_FOR`.
    pub(super) fn retain(&mut self, live: &[u64], now: i64) {
        let before = self.by_pane.len();
        self.by_pane.retain(|sid, m| live.contains(sid) && now - m.since < MARK_FOR.as_secs() as i64);
        self.dirty |= self.by_pane.len() != before;
    }

    /// Each marked pane, with when it was marked.
    pub(super) fn since_by_pane(&self) -> impl Iterator<Item = (u64, i64)> + '_ {
        self.by_pane.iter().map(|(sid, m)| (*sid, m.since))
    }

    /// Is this pane to be judged as a Fable pane, whatever its model?
    pub(super) fn marked(&self, sid: u64) -> bool {
        self.by_pane.contains_key(&sid)
    }

    /// What to tell the session moved from `from` to `to` for its
    /// refusal, or `None` once it has been told.
    pub(super) fn word(&self, sid: u64, from: u8, to: u8) -> Option<String> {
        let m = self.by_pane.get(&sid).filter(|m| !m.told)?;
        let what = match m.ask.agent.as_deref() {
            Some("") => "这个会话的一个后台子代理请求 Fable 时".to_string(),
            Some(name) => format!("这个会话的后台子代理「{name}」请求 Fable 时"),
            None => "这个会话请求 Fable 时".to_string(),
        };
        let again = if m.ask.agent.is_some() { "请重新启动这个子代理" } else { "请重新发出被拒的那次请求" };
        Some(format!(
            "marspot：{what}被账号 P{from} 拒绝了，P{from} 的 Fable 额度已经用完。\
             会话现在已经换到账号 P{to}，这个账号还有 Fable 额度。{again}，接着做之前的工作"
        ))
    }

    /// The session has been told, with a move that started.
    pub(super) fn told(&mut self, sid: u64) {
        if let Some(m) = self.by_pane.get_mut(&sid) {
            self.dirty |= !m.told;
            m.told = true;
        }
    }

    /// One line per pane, for the file that carries the marks across a
    /// restart; None when nothing changed since the last call.
    ///
    /// `sid  since  at  told  own|agent  [description]`: the
    /// description goes last, as the record holds it, escaped, so it
    /// has no tab or newline of its own.
    pub(super) fn changed(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        let mut lines: Vec<String> = self
            .by_pane
            .iter()
            .map(|(sid, m)| {
                let who = m.ask.agent.as_deref().map_or("own".to_string(), |a| format!("agent\t{a}"));
                format!("{sid}\t{}\t{}\t{}\t{who}\n", m.since, m.ask.at, u8::from(m.told))
            })
            .collect();
        lines.sort();
        Some(lines.concat())
    }

    /// What `changed` wrote, without marks older than `MARK_FOR` at
    /// `now`. A line that does not read is dropped.
    pub(super) fn decode(text: &str, now: i64) -> Self {
        let mut marks = Self::default();
        for line in text.lines() {
            let f: Vec<&str> = line.splitn(6, '\t').collect();
            let (Some(sid), Some(since), Some(at)) = (
                f.first().and_then(|s| s.parse::<u64>().ok()),
                f.get(1).and_then(|s| s.parse::<i64>().ok()),
                f.get(2).and_then(|s| s.parse::<i64>().ok()),
            ) else {
                continue;
            };
            let agent = match (f.get(4), f.get(5)) {
                (Some(&"own"), _) => None,
                (Some(&"agent"), name) => Some(name.unwrap_or(&"").to_string()),
                _ => continue,
            };
            if now - since >= MARK_FOR.as_secs() as i64 {
                continue;
            }
            let told = f.get(3) == Some(&"1");
            marks.by_pane.insert(sid, Mark { ask: Ask { at, agent }, since, told });
        }
        marks
    }
}

/// How the sweep judged a pane it is moving off its account.
pub(super) struct Judged {
    /// The model it is judged as: Fable for a marked pane.
    pub model: Option<String>,
    /// It is out only for want of the Fable it was refused; on its own
    /// model it could stay.
    pub for_fable: bool,
}

impl Judged {
    /// The model to choose the next account by.
    ///
    /// A marked pane that is out on its own model too still has to go
    /// somewhere it can work: when no other account has Fable left, it
    /// is placed by its own model instead of held where nothing answers.
    pub(super) fn room_model(&self, rooms: &[(u8, Room)], current: u8, own: Option<&String>) -> Option<String> {
        let fable_left = rooms.iter().any(|(p, r)| *p != current && *r != Room::Unknown && !r.refused_for(Some(FABLE)));
        if self.for_fable || fable_left || self.model.as_deref() == own.map(String::as_str) {
            self.model.clone()
        } else {
            own.cloned()
        }
    }

    /// The one line for a pane held where it is.
    pub(super) fn hold_line(&self, sid: u64, profile: u8, why: &str) -> (&'static str, String) {
        if self.for_fable && matches!(why, "every account is out" | "nowhere else to go") {
            let line = format!("sid={sid} stays on P{profile}: it was refused Fable and no other account has Fable left");
            return ("quota.fable_nowhere", line);
        }
        ("quota.hold", format!("sid={sid} stays on P{profile}: {why}"))
    }
}

impl ClaudecodePlugin {
    /// Mark a pane whose claude was refused Fable, and count a
    /// background agent's refusal as its account's, as a refusal of the
    /// session's own already is.
    pub(super) fn note_fable_refusal(&mut self, host: &dyn PluginHost, sid: u64, meta: &BindMeta, result: &ScanResult) {
        let Some(ask) = result.fable_refused.get(&sid) else { return };
        if let Some(r) = result.agent_refused.get(&sid).filter(|r| ask.agent.is_some() && r.at == ask.at)
            && self.refused.note(meta.profile_num, r)
        {
            host.log(LogLevel::Info, "quota.refused", &format!("P{} refused {} at {}", meta.profile_num, r.model, r.at));
        }
        self.fable_need.note(sid, ask, super::rooms::unix_now());
    }

    pub(super) fn load_fable_need(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(FILE)) {
            self.fable_need = Marks::decode(&text, super::rooms::unix_now());
        }
    }

    pub(super) fn persist_fable_need(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.fable_need.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = std::fs::write(dir.join(FILE), text) {
            host.log(LogLevel::Warn, "quota.fable_need_persist_failed", &format!("{e}"));
        }
    }
}

pub(super) const FILE: &str = "fable_need.tsv";

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_374_223;

    fn agent(name: &str) -> Ask {
        Ask { at: NOW, agent: Some(name.into()) }
    }

    #[test]
    fn a_mark_lasts_twelve_hours_and_ends_with_its_pane() {
        let mut marks = Marks::default();
        assert!(marks.note(1, &agent("a"), NOW));
        assert!(marks.note(2, &Ask { at: NOW, agent: None }, NOW));
        assert!(!marks.note(1, &agent("a"), NOW + 5), "the same refusal again");

        marks.retain(&[1, 2], NOW + MARK_FOR.as_secs() as i64 - 1);
        assert!(marks.marked(1) && marks.marked(2));

        // pane 2 closed
        marks.retain(&[1], NOW + 60);
        assert!(marks.marked(1) && !marks.marked(2));

        marks.retain(&[1], NOW + MARK_FOR.as_secs() as i64);
        assert!(!marks.marked(1), "twelve hours on");
    }

    #[test]
    fn marks_survive_being_written_and_read() {
        let mut marks = Marks::default();
        marks.note(7, &agent(r#"Design RFC: \"shadow\" subpanes"#), NOW);
        marks.note(8, &Ask { at: NOW - 30, agent: None }, NOW);
        marks.note(9, &agent(""), NOW - 100);
        marks.told(7);
        let text = marks.changed().expect("changed");
        assert_eq!(marks.changed(), None, "nothing new since");

        let back = Marks::decode(&text, NOW + 60);
        assert_eq!(back.by_pane, marks.by_pane);
        assert_eq!(back.word(7, 1, 2), None, "pane 7 was told");
        assert!(back.word(9, 1, 2).is_some_and(|w| w.contains("一个后台子代理")));

        // read back after twelve hours: gone
        let late = Marks::decode(&text, NOW - 100 + MARK_FOR.as_secs() as i64);
        assert!(late.marked(7) && late.marked(8) && !late.marked(9));
        assert!(Marks::decode("x\ty\n7\t1\t2\t0\tsomeone\n", NOW).by_pane.is_empty());
    }
}
