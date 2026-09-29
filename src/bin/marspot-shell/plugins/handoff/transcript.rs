//! What a handoff carries, independent of which agent it came from,
//! and how it is written down for the one it goes to.
//!
//! Deliberately little.  The repository already says what was done —
//! its files, `git status`, `git log` — and the next agent can read it
//! better than any list of tool calls could summarise it.  What the
//! repository does NOT hold is the conversation: what the user asked
//! for last and what the agent last told them.  That is the handoff:
//! the latest few exchanges, verbatim.

use std::path::PathBuf;

/// Which agent a history belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "codex",
        }
    }
    /// The word used in ledgers and menu labels.
    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
        }
    }
}

/// One exchange: what the user said, and the agent's final word on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    pub user: String,
    pub reply: String,
}

/// What was read out of one history for one handoff.
#[derive(Debug, Clone)]
pub struct Extract {
    pub source: Agent,
    pub session_id: String,
    pub history_path: PathBuf,
    pub turns: Vec<Turn>,
    /// Where reading stopped — the next handoff from this history
    /// starts here.
    pub end_offset: u64,
}

/// The marker every handoff message starts with.  A turn that begins
/// with it is left out when that side is read again, which is what
/// keeps A→B→A from handing B's copy of A back to A.
pub const MARKER: &str = "[marspot handoff]";

/// Exchanges carried.  The latest few are what the conversation is
/// about NOW; anything older the repository answers better.
pub const RECENT: usize = 3;
const USER_MAX: usize = 2_000;
const REPLY_MAX: usize = 3_000;

/// Cut `s` to at most `max` bytes on a char boundary, saying so.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes cut]", &s[..end], s.len() - end)
}

/// Quote a block of someone else's text.  Replies are markdown with
/// their own headings, which would otherwise read as this document's.
fn quoted(s: &str) -> String {
    s.lines()
        .map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The document.  `continuing` = the receiving agent already has this
/// conversation up to where it left it.
pub fn render(x: &Extract, continuing: bool) -> String {
    let src = x.source.display();
    let mut out = format!("# Handoff from {src}\n\n");
    out.push_str(if continuing {
        "You had this conversation before it moved to the other agent; these are \
         the latest exchanges there since you left."
    } else {
        "This conversation started in the other agent; these are its latest exchanges."
    });
    out.push_str(
        " The repository is the record of what was done — check `git status` and \
         `git log` rather than relying on this.\n\n",
    );
    let from = x.turns.len().saturating_sub(RECENT);
    if from > 0 {
        out.push_str(&format!("({from} earlier exchanges not included.)\n\n"));
    }
    for t in &x.turns[from..] {
        out.push_str(&format!("**User:**\n\n{}\n\n", quoted(&clip(&t.user, USER_MAX))));
        if t.reply.trim().is_empty() {
            out.push_str("**Reply:** (none — the turn ended without one)\n\n");
        } else {
            out.push_str(&format!("**Reply:**\n\n{}\n\n", quoted(&clip(&t.reply, REPLY_MAX))));
        }
    }
    out.push_str(&format!("Full history, if ever needed: `{}`\n", x.history_path.display()));
    out
}

/// The message typed into the receiving agent: one line, the substance
/// is in the document.
pub fn message(from: Agent, doc: &std::path::Path, continuing: bool) -> String {
    format!(
        "{MARKER} This pane just moved here from {} — {} is in {}. Read it, tell me in a few \
         sentences where things stand, then wait for my next instruction. Do not run commands \
         or change files until I ask.",
        from.display(),
        if continuing { "what happened there since you left" } else { "the conversation so far" },
        doc.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(user: &str, reply: &str) -> Turn {
        Turn { user: user.into(), reply: reply.into() }
    }

    fn extract(turns: Vec<Turn>) -> Extract {
        Extract {
            source: Agent::Claude,
            session_id: "s1".into(),
            history_path: PathBuf::from("/h/s1.jsonl"),
            turns,
            end_offset: 0,
        }
    }

    #[test]
    fn clip_stays_on_char_boundaries() {
        let s = "交接".repeat(100);
        let c = clip(&s, 10);
        assert!(c.starts_with("交接交")); // 9 bytes: three whole chars
        assert!(c.contains("more bytes cut"));
        assert_eq!(clip("short", 10), "short");
    }

    /// Only the latest exchanges travel, and the document stays small
    /// however long the session was.
    #[test]
    fn only_the_latest_exchanges_are_carried() {
        let big = "x".repeat(50_000);
        let mut turns: Vec<Turn> = (0..400).map(|i| turn(&format!("ask {i} {big}"), &big)).collect();
        turns.push(turn("the last ask", "the last reply"));
        let doc = render(&extract(turns), true);
        assert!(doc.contains("the last ask") && doc.contains("the last reply"));
        assert!(doc.contains("ask 398") && !doc.contains("ask 397"), "exactly {RECENT}");
        assert!(doc.contains("398 earlier exchanges not included"));
        assert!(doc.len() < 16 * 1024, "{} bytes", doc.len());
        assert!(doc.contains("git status") && doc.contains("/h/s1.jsonl"));
    }

    /// A reply's own headings must not become this document's sections.
    #[test]
    fn replies_are_quoted() {
        let doc = render(&extract(vec![turn("go", "## Done\n\n- one")]), false);
        assert!(doc.contains("> ## Done\n>\n> - one"), "{doc}");
        assert!(!doc.contains("\n## Done"));
    }

    #[test]
    fn the_message_is_marked_and_says_to_wait() {
        let m = message(Agent::Codex, std::path::Path::new("/s/handoff/7.md"), true);
        assert!(m.starts_with(MARKER));
        assert!(m.contains("/s/handoff/7.md") && m.contains("codex") && m.contains("wait"));
        assert!(!m.contains('\n'), "one line: it is typed into a TUI's input box");
    }
}
