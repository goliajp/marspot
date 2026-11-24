//! The helper table: derived from the event log, never written but by
//! applying an event, so replaying the log gives the same table.

use std::collections::BTreeMap;

use marspot::pidtree::ProcId;
use marspot_peer::control::{AgentKind, Isolation};

use super::strip::clock::Clock;
use super::turn::turn_reported;

/// How far back the notice rate looks.
pub(super) const NOTICE_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Notices one helper sends within the window at most.
pub(super) const NOTICES_PER_WINDOW: usize = 3;

/// What `started` records: everything decided before the agent runs.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Started {
    pub parent: u64,
    /// The agent that asked (`claude`, …) or `shell`.
    pub caller: String,
    pub agent: AgentKind,
    pub account: String,
    pub model: String,
    pub mode: String,
    pub isolation: Isolation,
    pub budget_min: u32,
    /// The task's first line, 120 characters at most.
    pub task_line: String,
    pub task_sha: String,
    pub label: String,
    /// The conversation in the parent pane when it was started; None:
    /// not known then, or an older build started it.
    pub conv: Option<String>,
    /// The pane at the top of its chain of parents; None: an older
    /// build started it.
    pub root: Option<u64>,
}

impl Started {
    /// Whether an agent asked for it: only those go when their parent
    /// pane has had no agent for a while.
    pub fn agent_caller(&self) -> bool {
        self.caller != "shell"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Helper {
    pub sid: u64,
    pub started: Started,
    pub started_ms: u64,
    pub preparing: bool,
    pub delivered_ms: Option<u64>,
    /// The account it runs on now.
    pub account: String,
    pub results_delivered: u32,
    /// The parent conversation it belongs to now: the one that started
    /// it, or what that was handed over to.
    pub conv: Option<String>,
    /// The agent that conversation was handed to, until it says which
    /// conversation it is in.
    pub heir: Option<ProcId>,
    /// The digest of the result text a notice last carried; empty: none
    /// did, or an older build sent it.
    pub result_sha: String,
    /// The result file changed since a notice last carried it.
    pub result_new: bool,
    /// A stop judged from the screen was told, and neither input nor a
    /// new result came since.
    pub noticed: bool,
    /// The last told stop was at a prompt, and nothing came since.
    pub blocked_told: bool,
    /// The digest of the prompt last told of; empty: not known.
    pub blocked_prompt: String,
    /// When notices went, within the last window.
    pub notices: Vec<u64>,
    /// Stops held back by the rate since the last notice.
    pub suppressed: u32,
    /// One of them was a stop at a prompt: the held notice says so.
    pub held_blocked: bool,
    /// Its program left the pane, and that was told.
    pub exited: bool,
    /// The pane at the top now, by what was last known.
    pub root: Option<u64>,
    /// How long it has been at work.
    pub clock: Clock,
}

impl Helper {
    pub fn new(sid: u64, started: Started, at_ms: u64) -> Self {
        let account = started.account.clone();
        let (conv, root) = (started.conv.clone(), started.root);
        Helper {
            sid,
            started,
            started_ms: at_ms,
            preparing: false,
            delivered_ms: None,
            account,
            results_delivered: 0,
            conv,
            heir: None,
            result_sha: String::new(),
            result_new: false,
            noticed: false,
            blocked_told: false,
            blocked_prompt: String::new(),
            notices: Vec::new(),
            suppressed: 0,
            held_blocked: false,
            exited: false,
            root,
            clock: Clock::started(at_ms),
        }
    }

    /// When its budget runs out, once its task went in.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.delivered_ms.map(|d| d + u64::from(self.started.budget_min) * 60_000)
    }

    /// Whether one more notice fits in the window ending at `now_ms`.
    pub fn may_notify(&self, now_ms: u64) -> bool {
        self.notices.iter().filter(|t| now_ms.saturating_sub(**t) < NOTICE_WINDOW_MS).count() < NOTICES_PER_WINDOW
    }

    /// When the window frees a place, if it is full at `now_ms`.
    pub fn window_opens_ms(&self, now_ms: u64) -> Option<u64> {
        (!self.may_notify(now_ms)).then(|| self.notices.iter().min().map_or(now_ms, |t| t + NOTICE_WINDOW_MS))
    }

    /// Given something, or wrote its result: judged from its screen, it
    /// is taken to be at work until it is seen to have stopped.
    fn prodded(&mut self, at_ms: u64) {
        if !turn_reported(self) {
            self.clock.begin(at_ms);
        }
    }

    fn noticed_at(&mut self, at_ms: u64) {
        self.notices.retain(|t| at_ms.saturating_sub(*t) < NOTICE_WINDOW_MS);
        self.notices.push(at_ms);
        self.suppressed = 0;
        self.held_blocked = false;
    }
}

/// One fact about one helper.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Event {
    Started(Started),
    Preparing,
    TaskDelivered,
    ResultChanged,
    /// `sha`: the digest of its text.
    ResultDelivered { sha: String },
    InputSeen,
    IdleNoticed,
    /// `prompt`: the digest of the prompt it is at; empty: not known.
    BlockedNoticed { prompt: String },
    ExitedNoticed,
    /// A stop the rate held back; `blocked`: at a prompt.
    NoticeSuppressed { blocked: bool },
    AccountChanged(String),
    /// Its parent was handed to this agent.
    ParentAgent(ProcId),
    /// Its parent conversation goes on as this one.
    ConversationChanged(String),
    /// Seen at work again after it had finished, and the other way.
    TurnBegan,
    TurnEnded,
    /// It is under this pane now.
    Rooted(u64),
    BudgetExpired,
    /// `exited`: its session was gone; nobody closed it.
    Stopped { exited: bool },
    /// Everything the events before a rotation said, in one line.
    Carried(Box<Helper>),
}

impl Event {
    pub fn word(&self) -> &'static str {
        match self {
            Event::Started(_) => "started",
            Event::Preparing => "preparing",
            Event::TaskDelivered => "task_delivered",
            Event::ResultChanged => "result_changed",
            Event::ResultDelivered { .. } => "result_delivered",
            Event::InputSeen => "input_seen",
            Event::IdleNoticed => "idle_noticed",
            Event::BlockedNoticed { .. } => "blocked_noticed",
            Event::ExitedNoticed => "exited_noticed",
            Event::NoticeSuppressed { .. } => "notice_suppressed",
            Event::AccountChanged(_) => "account_changed",
            Event::ParentAgent(_) => "parent_agent",
            Event::ConversationChanged(_) => "conversation_changed",
            Event::TurnBegan => "turn_began",
            Event::TurnEnded => "turn_ended",
            Event::Rooted(_) => "rooted",
            Event::BudgetExpired => "budget_expired",
            Event::Stopped { .. } => "stopped",
            Event::Carried(_) => "carried",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Table {
    pub map: BTreeMap<u64, Helper>,
}

impl Table {
    /// Apply one event that happened at `at_ms`.  An event for a helper
    /// with no `started` (one that went before a rotation) changes
    /// nothing.
    pub fn apply(&mut self, sid: u64, at_ms: u64, ev: &Event) {
        match ev {
            Event::Started(s) => {
                self.map.insert(sid, Helper::new(sid, s.clone(), at_ms));
                return;
            }
            Event::Carried(h) => {
                self.map.insert(sid, Helper { sid, ..(**h).clone() });
                return;
            }
            Event::Stopped { .. } => {
                self.map.remove(&sid);
                return;
            }
            _ => {}
        }
        let Some(h) = self.map.get_mut(&sid) else { return };
        match ev {
            Event::Preparing => h.preparing = true,
            Event::TaskDelivered => {
                h.preparing = false;
                h.delivered_ms = Some(at_ms);
            }
            Event::ResultChanged => {
                h.result_new = true;
                h.noticed = false;
                h.blocked_told = false;
                h.prodded(at_ms);
            }
            Event::ResultDelivered { sha } => {
                // told again as unchanged: no other result
                if sha.is_empty() || *sha != h.result_sha {
                    h.results_delivered += 1;
                }
                h.result_sha.clone_from(sha);
                h.result_new = false;
            }
            Event::InputSeen => {
                h.noticed = false;
                h.blocked_told = false;
                h.prodded(at_ms);
            }
            Event::IdleNoticed | Event::BlockedNoticed { .. } | Event::ExitedNoticed => {
                h.noticed = true;
                h.blocked_told = matches!(ev, Event::BlockedNoticed { .. });
                h.exited = matches!(ev, Event::ExitedNoticed);
                if let Event::BlockedNoticed { prompt } = ev {
                    h.blocked_prompt.clone_from(prompt);
                }
                h.noticed_at(at_ms);
                if h.exited {
                    h.clock.end(at_ms);
                }
            }
            Event::NoticeSuppressed { blocked } => {
                h.suppressed += 1;
                h.held_blocked |= *blocked;
            }
            Event::AccountChanged(a) => h.account = a.clone(),
            Event::ParentAgent(p) => h.heir = Some(*p),
            Event::ConversationChanged(c) => {
                h.conv = Some(c.clone());
                h.heir = None;
            }
            Event::TurnBegan => h.clock.begin(at_ms),
            Event::TurnEnded => h.clock.end(at_ms),
            Event::Rooted(r) => h.root = Some(*r),
            Event::BudgetExpired | Event::Started(_) | Event::Carried(_) | Event::Stopped { .. } => {}
        }
    }

    /// The one of two panes that is a helper the other started.
    pub fn helper_of_pair(&self, a: u64, b: u64) -> Option<&Helper> {
        let under = |helper: u64, parent| self.map.get(&helper).filter(|h| h.started.parent == parent);
        under(a, b).or_else(|| under(b, a))
    }

    /// The helpers under `parent`.
    pub fn under(&self, parent: u64) -> impl Iterator<Item = &Helper> {
        self.map.values().filter(move |h| h.started.parent == parent)
    }
}
