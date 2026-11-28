//! Every agent account's paid balance reading, as the adapter that
//! reads the records last published it.  The adapter replaces its
//! agent's rows after each pass; the usage pass reads a snapshot for
//! its feed, its samples and its check against another collector.

use marspot_agent::types::wallet::View;

use crate::host::published::Published;

/// One row per agent and account at most.
const MAX_ROWS: usize = 512;

pub(crate) static WALLETS: Published<Vec<(&'static str, u8, View)>> = Published::new();

/// Replace `agent`'s rows with what its last pass knew.
pub(in crate::plugins) fn publish(agent: &'static str, rows: impl IntoIterator<Item = (u8, View)>) {
    WALLETS.update(|t| {
        t.retain(|(a, _, _)| *a != agent);
        for (profile, view) in rows {
            if t.len() >= MAX_ROWS {
                break;
            }
            t.push((agent, profile, view));
        }
    });
}

/// Every account of `agent` with a reading, by account number.
pub(in crate::plugins) fn all(agent: &str) -> Vec<(u8, View)> {
    let Some(t) = WALLETS.get() else { return Vec::new() };
    t.iter().filter(|(a, _, _)| *a == agent).map(|(_, p, v)| (*p, v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_replaces_its_agents_rows() {
        const AGENT: &str = "test-wallet";
        publish(AGENT, [(1, View { balance: 3.0, ..View::default() }), (2, View::default())]);
        assert_eq!(all(AGENT).len(), 2);
        publish(AGENT, [(2, View { balance: 1.0, ..View::default() })]);
        assert_eq!(all(AGENT), vec![(2, View { balance: 1.0, ..View::default() })]);
    }
}
