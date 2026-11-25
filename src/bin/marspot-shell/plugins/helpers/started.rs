//! What a helper's start records: everything decided before its agent
//! runs, and how that is written on a record line and read back.

use marspot_peer::control::{AgentKind, Isolation};

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
    /// A few words on the task from whoever started it.
    pub title: String,
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

    /// Its arguments on a record line.
    pub fn kv(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("parent", self.parent.to_string()),
            ("caller", self.caller.clone()),
            ("agent", self.agent.as_str().into()),
            ("account", self.account.clone()),
            ("model", self.model.clone()),
            ("mode", self.mode.clone()),
            ("isolation", self.isolation.as_str().into()),
            ("budget", self.budget_min.to_string()),
            ("task", self.task_line.clone()),
            ("sha256", self.task_sha.clone()),
            ("label", self.label.clone()),
        ];
        v.extend(Some(self.title.clone()).filter(|t| !t.is_empty()).map(|t| ("title", t)));
        v.extend(self.conv.clone().map(|c| ("conv", c)));
        v.extend(self.root.map(|r| ("root", r.to_string())));
        v
    }

    /// What the arguments of a record line say, if they are whole.
    pub fn of(get: &dyn Fn(&str) -> Option<String>) -> Option<Started> {
        Some(Started {
            parent: get("parent")?.parse().ok()?,
            caller: get("caller")?,
            agent: AgentKind::parse(&get("agent")?)?,
            account: get("account").unwrap_or_default(),
            model: get("model").unwrap_or_default(),
            mode: get("mode").unwrap_or_default(),
            isolation: if get("isolation").as_deref() == Some("container") { Isolation::Container } else { Isolation::Host },
            budget_min: get("budget")?.parse().ok()?,
            task_line: get("task").unwrap_or_default(),
            task_sha: get("sha256").unwrap_or_default(),
            label: get("label").unwrap_or_default(),
            title: get("title").unwrap_or_default(),
            conv: get("conv"),
            root: get("root").and_then(|r| r.parse().ok()),
        })
    }
}
