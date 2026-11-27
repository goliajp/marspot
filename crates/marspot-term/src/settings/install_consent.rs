//! Whether marspot may install each agent CLI the first time it finds
//! it missing.

/// The person's answer for one CLI.
///
/// `ask` (the default) shows a question over the focused pane before
/// the first install; `yes` installs without asking; `no` never
/// installs it and stops marspot's own updates of it (claude and kimi
/// still update themselves).  With ask or yes, updates follow without
/// asking.  Every other word reads as ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallConsent {
    #[default]
    Ask,
    Yes,
    No,
}

impl InstallConsent {
    fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "yes" => Self::Yes,
            "no" => Self::No,
            _ => Self::Ask,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}

/// The CLIs marspot installs, by command name, in the order the
/// settings file and the panel list them.
pub const CLIS: [&str; 4] = ["claude", "codex", "gemini", "kimi"];

/// One answer per CLI of [`CLIS`], in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InstallConsents(pub [InstallConsent; 4]);

impl InstallConsents {
    /// The answer for the CLI named `cli`; None for a name not in [`CLIS`].
    pub fn of(&self, cli: &str) -> Option<InstallConsent> {
        CLIS.iter().position(|c| *c == cli).map(|i| self.0[i])
    }

    /// Set the answer for `cli`; false for a name not in [`CLIS`].
    pub fn set(&mut self, cli: &str, to: InstallConsent) -> bool {
        let Some(i) = CLIS.iter().position(|c| *c == cli) else { return false };
        self.0[i] = to;
        true
    }
}

/// The file's keys, `agents.install.<cli>`, in the order of [`CLIS`].
pub(super) const KEYS: [&str; 4] =
    ["agents.install.claude", "agents.install.codex", "agents.install.gemini", "agents.install.kimi"];

pub(super) fn value_of(c: &InstallConsents, key: &str) -> Option<String> {
    let i = KEYS.iter().position(|k| *k == key)?;
    Some(c.0[i].as_str().to_string())
}

/// Apply `key = v` when `key` is one of [`KEYS`]; false otherwise.
pub(super) fn parse_key(c: &mut InstallConsents, key: &str, v: &str) -> bool {
    let Some(i) = KEYS.iter().position(|k| *k == key) else { return false };
    c.0[i] = InstallConsent::parse(v);
    true
}

#[cfg(test)]
mod tests {
    use super::super::*;

    /// Installing the agent CLIs is on unless the file says otherwise,
    /// and saying otherwise survives a rewrite.
    #[test]
    fn agents_are_installed_unless_switched_off() {
        assert!(Settings::default().agents_install);
        let off = parse("agents.install = false\n");
        assert!(!off.agents_install);
        let kept = parse(&render(&off, "agents.install = false\n"));
        assert!(!kept.agents_install, "a rewrite keeps it off");
    }

    /// Each CLI is asked about unless the file answers for it, an answer
    /// survives a rewrite, and one CLI's answer is not another's.
    #[test]
    fn install_consent_is_ask_per_cli_unless_answered() {
        let s = Settings::default();
        for cli in INSTALL_CLIS {
            assert_eq!(s.install_consent.of(cli), Some(InstallConsent::Ask), "{cli}");
        }
        assert_eq!(s.install_consent.of("npm"), None);
        let body = "agents.install.codex = yes\nagents.install.kimi = No\nagents.install.gemini = maybe\n";
        let s = parse(body);
        assert_eq!(s.install_consent.of("codex"), Some(InstallConsent::Yes));
        assert_eq!(s.install_consent.of("kimi"), Some(InstallConsent::No));
        assert_eq!(s.install_consent.of("gemini"), Some(InstallConsent::Ask), "an unknown word asks");
        assert_eq!(s.install_consent.of("claude"), Some(InstallConsent::Ask));
        let back = render(&s, body);
        assert_eq!(parse(&back), s, "a rewrite keeps every answer");
        assert!(back.contains("agents.install.kimi = no\n") && back.contains("agents.install.claude = ask\n"), "{back}");
        let mut c = s.install_consent;
        assert!(c.set("claude", InstallConsent::No) && !c.set("npm", InstallConsent::Yes));
        assert_eq!(c.of("claude"), Some(InstallConsent::No));
        assert_eq!(c.of("codex"), Some(InstallConsent::Yes), "the others stay");
    }
}
