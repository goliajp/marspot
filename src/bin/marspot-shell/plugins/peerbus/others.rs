//! Every Codex process on this machine as far as it has been seen, in
//! a pane or not, and where it was running (`plugins::beside`).
//!
//! A pane's thread may be told by elimination only while its Codex is
//! the one Codex in its directory (`bind::assign`).  A daemon's
//! threads carry no process, so what is known is who was there.  A
//! Codex that started and exited between two looks is in neither; if
//! it opened a thread, that thread stays loaded for half an hour and
//! is counted as a second candidate once the pane's own exists, which
//! is where `assign` gives a pairing up.

use std::path::Path;

use marspot::pidtree::{self, ProcId, ProcTable};
use marspot_peer::json;

use crate::plugins::beside::{Beside, Kinds};

#[derive(Default)]
pub(super) struct Codexes {
    kinds: Kinds,
    seen: Beside,
}

impl Codexes {
    /// Look at the process table.  `daemons` are the app-server
    /// processes of the Codex homes: they sit in whatever directory
    /// the first TUI was started from, for as long as the home is in
    /// use.
    pub(super) fn look(&mut self, table: &ProcTable, daemons: &[ProcId], now_ms: u64, oldest_pane_ms: Option<u64>) {
        let alive = self
            .kinds
            .of(table, |row| crate::plugins::codex::looks_like_codex(row) && !pidtree::proc_cmdline(row.pid).is_some_and(|l| serves(&l)))
            .into_iter()
            .filter(|row| !daemons.contains(&row.id()))
            .map(|row| (row.id(), pidtree::proc_cwd(row.pid).map(|p| p.to_string_lossy().into_owned())))
            .collect();
        self.seen.note(alive, now_ms, oldest_pane_ms);
    }

    pub(super) fn alone(&self, me: ProcId, cwd: &str, since_ms: u64) -> bool {
        self.seen.alone(me, cwd, since_ms)
    }
}

/// A codex command line that runs a home's app-server or one of its
/// helpers (`app-server --listen …`, `app-server daemon pid-update-loop`),
/// not a TUI.  These sit in the directory of the TUI that started them,
/// whether or not the pid files name them (0.172.0 also runs the server
/// as a child of the TUI), and show no thread of their own.
fn serves(cmdline: &str) -> bool {
    cmdline.split(' ').nth(1) == Some("app-server")
}

/// The app-server and its updater of the Codex home `home`, from the
/// pid files Codex keeps there (0.163.0: `{"pid":…,"processIdentity":
/// {"startSeconds":…,"startMicroseconds":…}}`).
pub(super) fn daemons_of(home: &Path) -> Vec<ProcId> {
    ["daemon.pid", "daemon-updater.pid"]
        .iter()
        .filter_map(|f| std::fs::read_to_string(home.join("app-server-daemon").join(f)).ok())
        .filter_map(|text| daemon_in(&text))
        .collect()
}

fn daemon_in(pid_file: &str) -> Option<ProcId> {
    let v = json::parse(pid_file).ok()?;
    let at = v.get("processIdentity")?;
    let (secs, micros) = (at.get("startSeconds")?.i64()?, at.get("startMicroseconds")?.i64()?);
    Some(ProcId { pid: v.get("pid")?.i64()? as i32, start_us: secs as u64 * 1_000_000 + micros as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_read_from_its_pid_file() {
        let text = r#"{"pid":67903,"processStartTime":"Sat Nov 14 07:08:48 2026","processIdentity":{"bootId":"54FA","uniqueId":7408199,"startSeconds":1794607728,"startMicroseconds":860429},"executableIdentity":{"digest":[253]}}"#;
        assert_eq!(daemon_in(text), Some(ProcId { pid: 67903, start_us: 1_794_607_728_860_429 }));
        assert_eq!(daemon_in(r#"{"pid":1}"#), None);
        assert_eq!(daemon_in(""), None);
    }

    #[test]
    fn an_app_server_is_not_a_codex_beside_a_pane() {
        let bin = "/x/.codex-shared/packages/app-server-daemon/releases/0.172.0-aarch64-apple-darwin/bin/codex";
        assert!(serves(&format!("{bin} app-server --listen unix:// --analytics-default-enabled --managed-daemon")));
        assert!(serves(&format!("{bin} app-server daemon pid-update-loop")));
        assert!(!serves("codex"));
        assert!(!serves("codex resume 01a218a7-e30b-7f00-8df0-b35e1e443fe7"));
        assert!(!serves("codex exec review"));
    }
}
