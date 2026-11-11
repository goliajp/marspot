//! Which pane a recipient string means.  The one place that decides,
//! for the Claude Code hook, the MCP tools and marspot's own `--send`.
//!
//! A pane has a base name (its directory) and a four-digit code that
//! stays with it for life; `base#code` is its full name.  A recipient
//! is a base (`alt`), a full name (`alt#3381`, found by the code alone,
//! so it survives a change of directory), or either with the agent in
//! front (`claude:alt`, `claude:alt#3381`).  Nothing is guessed: a
//! string that fits several panes is refused with the candidates.

/// The agents a recipient can be narrowed to.
pub const KINDS: [&str; 4] = ["claude", "codex", "gemini", "kimi"];

/// One line teaching the forms, for tool descriptions and hook context.
pub const FORMS: &str = "Recipient forms: the plain pane name (`alt`) while only one pane has it; `name#code` (`alt#3381`), which never changes while the pane lives; `kind:name` (`claude:alt`, `codex:alt#3381`) to say which agent, kind being claude, codex, gemini or kimi.";

/// A pane as resolution sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target<'a> {
    /// The directory's name; for a pane without a code, the whole name
    /// it was listed under.
    pub base: &'a str,
    pub code: Option<u16>,
    /// The agent it runs, when known.
    pub kind: Option<&'a str>,
}

impl Target<'_> {
    pub fn full_name(&self) -> String {
        match self.code {
            Some(code) => format!("{}#{code}", self.base),
            None => self.base.to_string(),
        }
    }

    fn described(&self) -> String {
        match self.kind {
            Some(kind) => format!("{} ({kind})", self.full_name()),
            None => self.full_name(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index into the targets given.
    pub index: usize,
    /// Set when the pane was found by its code under another base:
    /// what it is called now.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Miss {
    /// For the sender to read.
    pub text: String,
    /// No pane fits at all, as opposed to several, or the wrong agent.
    pub unknown: bool,
}

struct Asked<'a> {
    kind: Option<&'static str>,
    base: &'a str,
    code: Option<u16>,
}

fn split(to: &str) -> Asked<'_> {
    let (kind, rest) = match to.split_once(':') {
        Some((k, rest)) if !rest.is_empty() => match KINDS.iter().find(|kind| kind.eq_ignore_ascii_case(k)) {
            Some(kind) => (Some(*kind), rest),
            None => (None, to),
        },
        _ => (None, to),
    };
    // a code needs a name in front: a bare `#390` is a session id on
    // marspot's command line
    let code = |c: &str| c.parse::<u16>().ok().filter(|n| c.len() == 4 && (1000..=9999).contains(n));
    match rest.rsplit_once('#') {
        Some((base, c)) if !base.is_empty() && code(c).is_some() => Asked { kind, base, code: code(c) },
        _ => Asked { kind, base: rest, code: None },
    }
}

/// Whether `to` is written in a form only pane names have: with a code
/// or an agent in front.
pub fn is_pane_form(to: &str) -> bool {
    let asked = split(to.trim());
    asked.kind.is_some() || asked.code.is_some()
}

fn same(a: &str, b: &str, fold: bool) -> bool {
    if fold { a.to_lowercase() == b.to_lowercase() } else { a == b }
}

/// The pane `to` means.
pub fn resolve(to: &str, targets: &[Target]) -> Result<Hit, Miss> {
    let to = to.trim();
    let asked = split(to);
    let named = |kind: Option<&str>, base: &str, fold: bool| -> Vec<usize> {
        (0..targets.len()).filter(|&i| same(targets[i].base, base, fold) && kind.is_none_or(|k| targets[i].kind == Some(k))).collect()
    };
    // as written first, then without regard to case
    for fold in [false, true] {
        // the whole string as a base before any split: a directory may
        // be called `kimi:notes`
        let whole = named(None, to, fold);
        if !whole.is_empty() {
            return one_of(to, &whole, targets, true);
        }
        if let Some(code) = asked.code {
            let coded: Vec<usize> = (0..targets.len()).filter(|&i| targets[i].code == Some(code)).collect();
            if !coded.is_empty() {
                return by_code(to, &asked, &coded, targets);
            }
        } else if asked.kind.is_some() {
            let hits = named(asked.kind, asked.base, fold);
            if !hits.is_empty() {
                return one_of(to, &hits, targets, false);
            }
        }
    }
    let mut text = format!("no pane is named {to:?}");
    if !targets.is_empty() {
        text.push_str(&format!(". The panes are: {}", list(&(0..targets.len()).collect::<Vec<_>>(), targets)));
    }
    Err(Miss { text, unknown: true })
}

fn list(hits: &[usize], targets: &[Target]) -> String {
    hits.iter().map(|&i| targets[i].described()).collect::<Vec<_>>().join(", ")
}

fn one_of(to: &str, hits: &[usize], targets: &[Target], offer_kinds: bool) -> Result<Hit, Miss> {
    if let [index] = hits {
        return Ok(Hit { index: *index, note: None });
    }
    let mut text = format!("{} panes are named {to:?}: {}. Send again to one of them", hits.len(), list(hits, targets));
    if offer_kinds {
        // an agent only one of them runs tells that one apart
        let alone: Vec<String> = hits
            .iter()
            .filter_map(|&i| targets[i].kind.map(|k| (i, k)))
            .filter(|(_, k)| hits.iter().filter(|&&j| targets[j].kind == Some(*k)).count() == 1)
            .map(|(i, k)| format!("{k}:{}", targets[i].base))
            .collect();
        if !alone.is_empty() {
            text.push_str(&format!(", or to {}", alone.join(" or ")));
        }
    }
    text.push('.');
    Err(Miss { text, unknown: false })
}

fn by_code(to: &str, asked: &Asked, coded: &[usize], targets: &[Target]) -> Result<Hit, Miss> {
    let [index] = coded else {
        return one_of(to, coded, targets, false);
    };
    let t = &targets[*index];
    if let Some(want) = asked.kind
        && t.kind != Some(want)
    {
        let text = match t.kind {
            Some(runs) => format!("{} runs {runs}, not {want}", t.full_name()),
            None => format!("{} does not run {want}", t.full_name()),
        };
        return Err(Miss { text, unknown: false });
    }
    let note = (!same(asked.base, t.base, true)).then(|| format!("{}#{} is now called {}", asked.base, asked.code.unwrap_or_default(), t.full_name()));
    Ok(Hit { index: *index, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t<'a>(base: &'a str, code: u16, kind: &'a str) -> Target<'a> {
        Target { base, code: Some(code), kind: Some(kind) }
    }

    fn hit(to: &str, targets: &[Target]) -> usize {
        resolve(to, targets).unwrap_or_else(|m| panic!("{to}: {}", m.text)).index
    }

    fn miss(to: &str, targets: &[Target]) -> Miss {
        resolve(to, targets).expect_err(to)
    }

    #[test]
    fn a_lone_pane_answers_to_every_form() {
        let ts = [t("marspot", 1024, "claude"), t("alt", 3381, "kimi")];
        for to in ["marspot", "marspot#1024", "claude:marspot", "claude:marspot#1024", " Marspot ", "CLAUDE:MARSPOT"] {
            assert_eq!(hit(to, &ts), 0, "{to}");
        }
        assert_eq!(hit("kimi:alt", &ts), 1);
    }

    #[test]
    fn two_panes_of_one_name_and_different_agents() {
        let ts = [t("marspot", 1024, "claude"), t("marspot", 9906, "codex")];
        let m = miss("marspot", &ts);
        assert_eq!(
            m.text,
            "2 panes are named \"marspot\": marspot#1024 (claude), marspot#9906 (codex). Send again to one of them, or to claude:marspot or codex:marspot."
        );
        assert!(!m.unknown);
        assert_eq!(hit("claude:marspot", &ts), 0);
        assert_eq!(hit("codex:marspot", &ts), 1);
        assert_eq!(hit("marspot#1024", &ts), 0);
        assert_eq!(hit("marspot#9906", &ts), 1);
    }

    #[test]
    fn two_panes_of_one_name_and_one_agent() {
        let ts = [t("marspot", 1024, "claude"), t("marspot", 4410, "claude"), t("marspot", 9906, "codex")];
        assert_eq!(miss("claude:marspot", &ts).text, "2 panes are named \"claude:marspot\": marspot#1024 (claude), marspot#4410 (claude). Send again to one of them.");
        // only the agent that tells one pane apart is offered
        assert_eq!(
            miss("marspot", &ts).text,
            "3 panes are named \"marspot\": marspot#1024 (claude), marspot#4410 (claude), marspot#9906 (codex). Send again to one of them, or to codex:marspot."
        );
        assert_eq!(hit("codex:marspot", &ts), 2);
    }

    #[test]
    fn a_closed_pane_leaves_its_twin_the_plain_name_and_takes_its_code_with_it() {
        let ts = [t("marspot", 1024, "claude")];
        assert_eq!(hit("marspot", &ts), 0);
        assert_eq!(hit("marspot#1024", &ts), 0);
        let m = miss("marspot#9906", &ts);
        assert!(m.unknown);
        assert_eq!(m.text, "no pane is named \"marspot#9906\". The panes are: marspot#1024 (claude)");
        assert_eq!(miss("x", &[]).text, "no pane is named \"x\"");
    }

    #[test]
    fn a_full_name_follows_the_pane_to_another_directory() {
        let ts = [t("alt", 1024, "claude")];
        let h = resolve("marspot#1024", &ts).unwrap();
        assert_eq!((h.index, h.note.as_deref()), (0, Some("marspot#1024 is now called alt#1024")));
        assert_eq!(resolve("alt#1024", &ts).unwrap().note, None);
        // the old base alone no longer names it
        assert!(miss("marspot", &ts).unknown);
    }

    #[test]
    fn a_full_name_with_the_wrong_agent_is_refused() {
        let ts = [t("marspot", 1024, "codex")];
        let m = miss("claude:marspot#1024", &ts);
        assert_eq!((m.text.as_str(), m.unknown), ("marspot#1024 runs codex, not claude", false));
        let unknown = [Target { base: "marspot", code: Some(1024), kind: None }];
        assert_eq!(miss("claude:marspot#1024", &unknown).text, "marspot#1024 does not run claude");
        assert_eq!(hit("marspot#1024", &unknown), 0);
    }

    #[test]
    fn a_directory_named_like_an_agent_prefix_is_a_base_first() {
        let ts = [t("kimi:notes", 2000, "claude"), t("notes", 3000, "kimi")];
        assert_eq!(hit("kimi:notes", &ts), 0);
        assert_eq!(hit("kimi:notes#3000", &ts), 1);
    }

    #[test]
    fn only_a_four_digit_suffix_after_a_name_is_a_code() {
        let ts = [t("spg", 1024, "claude")];
        for to in ["spg#1", "spg#2", "#1024", "spg#10240", "spg#0999", "sp", "goliajp/spg", ""] {
            assert!(miss(to, &ts).unknown, "{to}");
        }
        assert!(is_pane_form("spg#1024") && is_pane_form("codex:spg") && !is_pane_form("spg") && !is_pane_form("spg#2") && !is_pane_form("other:spg"));
    }

    /// A pane listed by a marspot that knows no codes is found by the
    /// name it was listed under, and by nothing else.
    #[test]
    fn a_pane_without_a_code_is_found_by_its_listed_name() {
        let ts = [Target { base: "spg#2", code: None, kind: Some("codex") }, t("gds", 5000, "claude")];
        assert_eq!(hit("spg#2", &ts), 0);
        assert!(miss("spg", &ts).unknown);
    }
}
