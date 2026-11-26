//! How the last claim of each account went, for its card on the
//! accounts panel, read back from the claim facts the shell writes to
//! `usage/reset_claims/YYYYMMDD.tsv` (one file per UTC day, a header
//! first; columns `at, request_id, provider, profile, kind, grant_ref,
//! by, result, left_after, detail, …`).

use std::path::{Path, PathBuf};

/// No more accounts than this have a claim shown.
const MAX_SHOWN: usize = 64;

/// One account's last claim.
#[derive(Clone, Debug, PartialEq)]
pub struct Claimed {
    /// Unix seconds.
    pub at: i64,
    pub agent: String,
    pub profile: u8,
    pub kind: String,
    /// One of the claim's result words: `reset`, `already_used`, …
    pub result: String,
    pub detail: String,
}

pub fn claims_dir() -> PathBuf {
    marspot_term::paths::state_root().join("usage/reset_claims")
}

fn row(line: &str) -> Option<Claimed> {
    let c: Vec<&str> = line.split('\t').collect();
    let (at, agent, profile, kind, result, detail) = (c.first()?, c.get(2)?, c.get(3)?, c.get(4)?, c.get(7)?, c.get(9)?);
    Some(Claimed { at: at.parse().ok()?, agent: agent.to_string(), profile: profile.parse().ok()?, kind: kind.to_string(), result: result.to_string(), detail: detail.to_string() })
}

/// The last claim of each account in the newest two day files under
/// `dir`; a claim from before that is older than any reading shown.
pub fn last_claims(dir: &Path) -> Vec<Claimed> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "tsv")).collect();
    files.sort();
    let mut out: Vec<Claimed> = Vec::new();
    for f in files.iter().skip(files.len().saturating_sub(2)) {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for c in text.lines().skip(1).filter_map(row) {
            match out.iter().position(|o| o.agent == c.agent && o.profile == c.profile) {
                Some(i) if out[i].at <= c.at => out[i] = c,
                Some(_) => {}
                None if out.len() < MAX_SHOWN => out.push(c),
                None => {}
            }
        }
    }
    out
}

/// The text of the reset line that claim `c` was for, while no reading
/// newer than it has come: `full reset: reset`.
pub fn claimed_text(c: &Claimed) -> String {
    let shown = c.kind.strip_prefix("codex_").unwrap_or(&c.kind);
    if c.detail.is_empty() || c.detail == c.result { format!("{shown} {}", c.result) } else { format!("{shown} {}: {}", c.result, c.detail) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "at\trequest_id\tprovider\tprofile\tkind\tgrant_ref\tby\tresult\tleft_after\tdetail\treason\tagain\tobserved_after\n";

    #[test]
    fn each_account_shows_its_last_claim_from_the_newest_day_files() {
        let dir = marspot_term::test_env::TempDir::new("claimed-last");
        let day = |name: &str, rows: &[&str]| std::fs::write(dir.join(name), format!("{HEAD}{}\n", rows.join("\n"))).unwrap();
        day("20261120.tsv", &["100\tr0\tcodex\t9\tcodex_full\tcodex:9:g1\tmarspot-ui\treset\t\treset\tx\t\t"]);
        day("20261126.tsv", &["200\tr1\tcodex\t1\tcodex_full\tcodex:1:g1\tmarspot-ui\terror\t\tthe app-server did not answer\tx\t1\t"]);
        day("20261127.tsv", &[
            "300\tr2\tcodex\t1\tcodex_full\tcodex:1:g1\tmarspot-ui\treset\t3\treset\tx\t\t{}",
            "310\tr3\tclaude\t3\tsession\tclaude:3:session\tdevops-ui\tunavailable\t\tA 5-hour limit reset isn't available right now.\tx\t\t",
        ]);
        let got = last_claims(dir.path());
        assert_eq!(got.len(), 2, "{got:?}");
        let codex = got.iter().find(|c| c.agent == "codex").unwrap();
        assert_eq!((codex.at, codex.profile, codex.result.as_str()), (300, 1, "reset"));
        assert!(!got.iter().any(|c| c.profile == 9), "older than the newest two days");
        assert_eq!(claimed_text(codex), "full reset");
        let claude = got.iter().find(|c| c.agent == "claude").unwrap();
        assert_eq!(claimed_text(claude), "session unavailable: A 5-hour limit reset isn't available right now.");
        assert!(last_claims(&dir.join("none")).is_empty());
    }
}
