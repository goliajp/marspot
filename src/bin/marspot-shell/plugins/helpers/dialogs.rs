//! The screens a new agent puts up before its input box, read off the
//! screen's text: which one it is, its choices and which of them the
//! cursor is on, and the directories it asks about.
//!
//! A dialog counts only as the last thing on the screen, under its own
//! question and with no input box: the task, once it runs, is shown
//! above the input box, and text in it that reads like a dialog is not
//! one.

use std::path::Path;

use marspot_peer::control::AgentKind;

const CLAUDE_TRUST: &str = "Is this a project you created or one you trust";
const CLAUDE_DIR: &str = "Accessing workspace:";
const CLAUDE_BYPASS: &str = "Bypass Permissions mode";
const CODEX_TRUST: &str = "Trust this folder?";
const CODEX_DIR: &str = "Folder access";
/// codex, below a repository's root, trusts the root.
const CODEX_ROOT: &str = "repository root:";
/// Its note on that, without the spaces: it wraps anywhere.
const CODEX_NOTE: [&str; 2] = ["subdirectoryofaGitproject", "repositoryroot"];
/// gemini asks about the directories `--include-directories` adds even
/// with `--skip-trust`.
const GEMINI_DIRS: &str = "Do you trust the following folders being added";
const GEMINI_DIRS_END: &str = "Trusting a folder";

const MARKERS: [char; 4] = ['❯', '›', '●', '>'];
/// Choices a dialog has at most.
const MAX_OPTIONS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ask {
    /// Whether to trust directories.
    Trust,
    /// claude's question about running with no questions.
    Bypass,
}

impl Ask {
    pub fn word(self) -> &'static str {
        match self {
            Ask::Trust => "trust",
            Ask::Bypass => "bypass",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Dialog {
    pub ask: Ask,
    pub options: Vec<String>,
    /// The choice the cursor is on.
    pub cursor: usize,
    pub yes: usize,
    /// Each directory it names, as the rows it is drawn on.
    pub dirs: Vec<Vec<String>>,
}

/// What to press next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Press {
    Down,
    Up,
    Enter,
}

impl Press {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Press::Down => b"\x1b[B",
            Press::Up => b"\x1b[A",
            Press::Enter => b"\r",
        }
    }
}

impl Dialog {
    /// Enter only with the cursor on yes.
    pub fn press(&self) -> Press {
        match self.yes.checked_sub(self.cursor) {
            Some(0) => Press::Enter,
            Some(_) => Press::Down,
            None => Press::Up,
        }
    }

    /// Whether saying yes trusts `dir` and nothing else.
    pub fn is_about(&self, dir: &Path) -> bool {
        match self.ask {
            Ask::Bypass => true,
            Ask::Trust => !self.dirs.is_empty() && self.dirs.iter().all(|rows| names(rows, dir)),
        }
    }

    /// The directories it names, for a log.
    pub fn dirs_text(&self) -> String {
        let one = |rows: &Vec<String>| rows.concat().chars().filter(|c| !c.is_control()).collect::<String>();
        self.dirs.iter().map(one).collect::<Vec<_>>().join(", ")
    }
}

/// What a still screen shows.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Look {
    /// The agent's input box.
    Input,
    Ask(Dialog),
    Other,
}

/// `input`: what the pane's input box is read to hold, when one is.  A
/// choice under the cursor is drawn like one, with the same glyph.
pub(super) fn look(agent: AgentKind, screen: &str, input: Option<&str>) -> Look {
    match (dialog(agent, screen), input) {
        (Some(d), None) => Look::Ask(d),
        (Some(d), Some(t)) if t.lines().next().is_some_and(|l| choice(l) == d.options[d.cursor]) => Look::Ask(d),
        (_, Some(_)) => Look::Input,
        (None, None) => Look::Other,
    }
}

/// A row without the sides of the box it is drawn in.
fn bare(row: &str) -> &str {
    row.trim().trim_matches(['│', '┃']).trim()
}

/// A choice without its number.
fn choice(text: &str) -> &str {
    let t = text.trim();
    match t.split_once(". ") {
        Some((n, rest)) if n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()) => rest.trim(),
        _ => t,
    }
}

/// The choices at the bottom of the screen: the row they start on, the
/// choices, and the one the cursor is on.
fn choices(rows: &[&str]) -> Option<(usize, Vec<String>, usize)> {
    let at = rows.iter().rposition(|r| r.starts_with(MARKERS))?;
    let first = rows[..at].iter().rposition(|r| r.is_empty()).map_or(0, |i| i + 1);
    let end = rows[at..].iter().position(|r| r.is_empty()).map_or(rows.len(), |i| at + i);
    let block = &rows[first..end];
    if block.len() > MAX_OPTIONS || block.iter().filter(|r| r.starts_with(MARKERS)).count() != 1 {
        return None;
    }
    let options = block.iter().map(|r| choice(r.trim_start_matches(MARKERS)).to_string()).collect();
    below_is_hint(&rows[end..]).then_some((first, options, at - first))
}

/// Nothing is under the choices but a line on the keys.
fn below_is_hint(rows: &[&str]) -> bool {
    let mut text = rows.iter().filter(|r| !r.is_empty());
    text.next().is_none_or(|r| r.to_ascii_lowercase().starts_with("enter ")) && text.next().is_none()
}

fn dialog(agent: AgentKind, screen: &str) -> Option<Dialog> {
    let all: Vec<&str> = screen.lines().map(bare).collect();
    // gemini keeps its status rows under the dialog's box
    let box_end = all.iter().rposition(|r| r.starts_with('╰')).filter(|_| agent == AgentKind::Gemini);
    let rows = &all[..box_end.unwrap_or(all.len())];
    let (first, options, cursor) = choices(rows)?;
    let above = &rows[..first];
    let text = above.join(" ");
    let only = |is: fn(&str) -> bool| {
        let mut found = options.iter().enumerate().filter(|(_, o)| is(o)).map(|(i, _)| i);
        found.next().filter(|_| found.next().is_none())
    };
    let (ask, yes, dirs) = match agent {
        AgentKind::Claude if options.iter().any(|o| o == "No, exit") => {
            let yes = only(|o| o.starts_with("Yes,"))?;
            if text.contains(CLAUDE_TRUST) {
                (Ask::Trust, yes, after(above, CLAUDE_DIR).into_iter().collect())
            } else if text.contains(CLAUDE_BYPASS) && options[yes] == "Yes, I accept" {
                (Ask::Bypass, yes, Vec::new())
            } else {
                return None;
            }
        }
        AgentKind::Codex if text.contains(CODEX_TRUST) => {
            (Ask::Trust, only(|o| o == "Trust and continue")?, codex_dirs(above, &text))
        }
        AgentKind::Gemini if text.contains(GEMINI_DIRS) => (Ask::Trust, only(|o| o == "Yes")?, gemini_dirs(above)),
        _ => return None,
    };
    Some(Dialog { ask, options, cursor, yes, dirs })
}

/// The paragraph after the last row that ends in `head`.
fn after(rows: &[&str], head: &str) -> Option<Vec<String>> {
    let at = rows.iter().rposition(|r| r.ends_with(head))?;
    let para = rows[at + 1..].iter().skip_while(|r| r.is_empty()).take_while(|r| !r.is_empty());
    Some(para.map(|r| r.to_string()).collect())
}

/// The folder codex asks about, and the repository's root when that is
/// what it would trust; none when its note is there with no root to read.
fn codex_dirs(rows: &[&str], text: &str) -> Vec<Vec<String>> {
    let text: String = text.split_whitespace().collect();
    let mut dirs: Vec<Vec<String>> = after(rows, CODEX_DIR).into_iter().collect();
    if CODEX_NOTE.iter().any(|n| text.contains(n)) {
        match after(rows, CODEX_ROOT).filter(|_| !dirs.is_empty()) {
            Some(root) => dirs.push(root),
            None => dirs.clear(),
        }
    }
    dirs
}

/// The `- directory` items under gemini's question.
fn gemini_dirs(rows: &[&str]) -> Vec<Vec<String>> {
    let Some(at) = rows.iter().rposition(|r| r.contains(GEMINI_DIRS)) else { return Vec::new() };
    let mut out: Vec<Vec<String>> = Vec::new();
    for r in rows[at + 1..].iter().take_while(|r| !r.starts_with(GEMINI_DIRS_END)) {
        match r.strip_prefix('-').filter(|rest| rest.is_empty() || rest.starts_with(' ')) {
            Some(rest) => out.push(vec![rest.trim().to_string()]),
            None => out.last_mut().into_iter().for_each(|d| d.push(r.to_string())),
        }
    }
    out
}

/// Whether `rows` together are `dir`, as it is written or as it
/// resolves.  A row may have ended at a space of the path.
fn names(rows: &[String], dir: &Path) -> bool {
    let is = |p: &Path| {
        let Some(mut rest) = p.to_str() else { return false };
        for r in rows {
            let Some(left) = rest.strip_prefix(r.as_str()) else { return false };
            rest = left.trim_start_matches(' ');
        }
        rest.is_empty()
    };
    let Ok(real) = std::fs::canonicalize(dir) else { return false };
    // `/tmp` is a link to `/private/tmp`; a program may say either
    let short = real.strip_prefix("/private").ok().map(|p| Path::new("/").join(p));
    let short = short.filter(|p| std::fs::canonicalize(p).is_ok_and(|c| c == real));
    is(dir) || is(&real) || short.is_some_and(|p| is(&p))
}
