//! Every file under the state root (`marspot_term::paths::state_root`):
//! where it is, what kind of state it holds, who writes it, how far it
//! may grow, and whether a build other than the one that wrote it uses
//! it -- after an update, or from another binary.  A file in that last
//! column cannot change its format, name or locking without a
//! mixed-version test.
//!
//! "Uses" is reads, connects to, locks, appends to, or acts on by its
//! name (deletes by the age in it, renames).  A file only ever replaced
//! without being read, or written and read back in one call, is not
//! used across images; neither is a file no build writes and none
//! reads.  When in doubt the answer is yes: a wrong no skips a test.
//!
//! Paths are relative to the state root, in the syntax of [`pattern`].
//! The first row that matches a path is its row.  `logs/` is under the
//! state root only in a sandbox; the installed app logs to
//! `~/Library/Logs/Marspot` in the same layout.
//!
//! Two tests keep the table whole: one lists a sandbox's state root
//! after a run of the app, the other reads the source for paths built
//! on the state root.

pub(crate) mod pattern;
#[cfg(test)]
mod source_scan;
#[cfg(test)]
mod tests;

/// What a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// What happened: only ever appended to, unless its row is in
    /// [`NOT_APPEND_ONLY`] or [`ROTATED`].
    Fact,
    /// Computed from facts or read from the outside world; deleting it
    /// loses nothing that cannot be had again.
    Derived,
    /// Working state that is rewritten in place and cannot be rebuilt:
    /// layouts, ledgers, a pane's terminal history, staged binaries.
    Draft,
    /// What the user set.
    UserConfig,
    /// Written by, or for, a program that is not marspot.
    External,
    /// Means something only while a process lives: sockets, locks, pid
    /// files, the temporary half of an atomic replace.
    Runtime,
}

/// One row of the table.
#[derive(Debug)]
pub(crate) struct Entry {
    pub path: &'static str,
    pub kind: Kind,
    pub writer: &'static str,
    pub cap: &'static str,
    /// Read by a build other than the one that wrote it.
    pub across_images: bool,
}

use Kind::*;

const fn e(path: &'static str, kind: Kind, writer: &'static str, cap: &'static str, across_images: bool) -> Entry {
    Entry { path, kind, writer, cap, across_images }
}

/// Written by no current code: what earlier builds left in an
/// installed app's state root.
const LEFT_BEHIND: &str = "none (left by an older build)";

pub(crate) const STATE_MAP: &[Entry] = &[
    // the layers' own files
    e("settings.toml", UserConfig, "settings panel (marspot_term::settings), the user", "the known keys", true),
    e("shell-state.bin", Draft, "L2 marspot::state (layout)", "one record per window", true),
    e("window-state.bin", Draft, "L2 marspot::state (geometry)", "one record per window", true),
    e("sessions/.next_id", Draft, "marspot_term::session_registry", "one number", true),
    e("sessions/<id>/entry.toml", Draft, "L3, L2 (session_registry::entry)", "one per pane", true),
    // L2 of any build connects to the L3 that bound it
    e("sessions/<id>/sock", Runtime, "L3 uds_server", "one per pane", true),
    // L2 tries it to tell whether an L3, maybe of another build, holds the pane
    e("sessions/<id>/.lock", Runtime, "L3 (session_registry::try_lock_session_dir)", "one per pane", true),
    e("sessions/<id>/guard", Runtime, "L3 owner_watch", "one per pane", true),
    e("sessions/<id>/closed", Runtime, "session_registry::closed", "one per pane", true),
    e("sessions/<id>/state.bin", Derived, "L3 snapshot_writer", "one per pane", true),
    e("sessions/<id>/bytelog.seg.<n>", Draft, "L3 bytelog (segment ring)", "100 MiB per pane", true),
    e("sessions/<id>/bytelog", Draft, "older L3 (rotating pair, read and deleted)", "50 MiB", true),
    e("sessions/<id>/bytelog.1", Draft, "older L3 (rotating pair, read and deleted)", "50 MiB", true),
    // an older build left it, today's L3 deletes it by name
    e("sessions/<id>/bytelog.1.old", Draft, "older L3 (deleted on open)", "50 MiB", true),
    e("sessions/<id>/scrollback.seg.<n>", Derived, "L3 scrollback::file (segment ring)", "scrollback tiers", true),
    e("sessions/<id>/scrollback.bin", Derived, "older L3 (read, then replaced by segments)", "scrollback tiers", true),
    e("sessions/<id>/scrollback.idx", Derived, "older L3 (read, then replaced by segments)", "scrollback tiers", true),
    e("sessions/<id>/scrollback.cold.bin", Derived, "older L3 (read, then replaced by segments)", "scrollback tiers", true),
    e("sessions/<id>/scrollback.cold.idx", Derived, "older L3 (read, then replaced by segments)", "scrollback tiers", true),
    e("sessions/<id>/scrollback*.bin.marks", Derived, "older L3 (sidecar, read with its pair)", "scrollback tiers", true),
    e("sessions/<id>/scrollback*.bin.clusters", Derived, "older L3 (sidecar, read with its pair)", "scrollback tiers", true),
    e("sessions/<id>/scrollback*.bin.clustertext", Derived, "older L3 (sidecar, read with its pair)", "scrollback tiers", true),
    // a pair the L3 could not open, set aside and read by nothing
    e("sessions/<id>/scrollback.bin.corrupt-<n>", Draft, "L3 Scrollback::file (quarantine)", "one per refused open", false),
    e("sessions/<id>/scrollback.idx.corrupt-<n>", Draft, "L3 Scrollback::file (quarantine)", "one per refused open", false),
    e("retired/<id>-<n>/**", Draft, "session_registry::removal (retired pane)", "14 days", true),
    // a taker that died leaves it; any later build purges it by the age in its name
    e("retired/<id>.<n>-<n>/**", Draft, "session_registry::removal (taken to delete)", "deleted by the taker, else 14 days", true),
    e("binaries/current/*", Draft, "L1 supervisor, install-local.sh", "one per binary", true),
    e("binaries/prev/*", Draft, "L1 supervisor", "one per binary", true),
    e("binaries/pending/*", Draft, "updater, install-local.sh", "one per binary", true),
    // only ever renamed over, never read
    e("binaries/quarantine/*", Draft, "L1 supervisor", "one per binary", false),
    // the download files are written and read within one update check
    e("download/marspot.tar.gz", Runtime, "updater", "one release", false),
    e("download/marspot.tar.gz.sig", Runtime, "updater", "one signature", false),
    e("download/marspot-update.pub", Derived, "updater::verify (from the built-in keys)", "fixed", false),
    e("download/marspot-update.<n>.pub", Derived, "updater::verify (from the built-in keys)", "fixed", false),
    e("download/marspot.tar.d/**", Runtime, "updater (the release unpacked)", "one release", false),
    e("pending-bundle/**", Draft, "install-local.sh, its land.sh", "one bundle", true),
    // trimmed to its last rows in place, so working state rather than a fact
    e("shell_launches.tsv", Draft, "L1 launch (crash-loop journal)", "last rows, trimmed in place", true),
    e("shell.pid", Runtime, "L1 launch", "one number", true),
    // the next L1, of whatever build, takes the lock or hands over through the socket
    e("shell.lock", Runtime, "L1 instance", "empty", true),
    e("front.sock", Runtime, "L1 instance", "socket", true),
    // `marspot-shell --send / --panes` may be another build than the running L1
    e("l1-cmd.sock", Runtime, "L1 cli_socket", "socket", true),
    // marspot-peer, installed on its own and kept across updates, connects
    e("control.sock", Runtime, "L1 pane_control", "socket", true),
    e("control-audit.log", Fact, "L1 pane_control::audit_log", "8 MiB, rotated", true),
    e("control-audit.log.<n>", Fact, "L1 pane_control::audit_log (rotated)", "7 files", true),
    e("control-deny.tsv", UserConfig, "control_deny (the user's refusals)", "one row per rule", true),
    // the receipts hook runs another binary
    e("submit-receipts.sock", Runtime, "L1 receipts", "socket", true),
    e("submit-receipt.key", Runtime, "marspot_term::submit_receipt", "one key", true),
    e("owner", Runtime, "marspot_term::owner (L1)", "one record", true),
    e("owner.heir", Runtime, "marspot_term::owner, L1 identity", "one record", true),
    // every binary that may spawn the owner takes it
    e("owner.spawn", Runtime, "marspot_term::owner", "lock", true),
    e("owner.reseats", Runtime, "L1 reclaim", "one record", true),
    e("bundle", Runtime, "L1 ownership", "one path", true),
    e("pane-codes", Draft, "marspot_term::pane_code", "one row per pane", true),
    // every binary that hands out pane codes takes it
    e("pane-codes.lock", Runtime, "marspot_term::pane_code", "lock", true),
    e("pane-state-clock.tsv", Draft, "L1 pane_status", "one row per pane", true),
    e("autorun.tsv", UserConfig, "L1 cli_socket (autorun switch)", "one row per directory", true),
    e("system-theme", Derived, "marspot_term::system_theme (L1)", "one word", true),
    e("system-language", Derived, "marspot_term::system_language (L1)", "one word", true),
    // the two markers are made by a person or a test, never by a build
    e("supervisor-profile", UserConfig, "a developer (marker read by L1 supervisor_profile)", "empty", false),
    e("dev-ae-drop", UserConfig, "a test (marker read by L1 apple_events::seam)", "one line", false),
    // every binary of every build appends, rotates and ages out the logs
    e("logs/marspot.log", Fact, "logx, every binary", "size cap, rotated", true),
    e("logs/marspot.<n>.log", Fact, "logx rotate", "logx gc by age", true),
    e("logs/marspot.<n>.log.gz", Fact, "logx rotate", "logx gc by age", true),
    e("logs/marspot.log.rotate-lock", Runtime, "logx rotate", "lock", true),
    // older builds wrote it, today's `marspot-shell --status` reads it
    e("logs/supervisor.log", Fact, "none now; read by marspot-shell --status", "none written", true),
    // madcawl's output; the L1 of every build opens it to append and empties it past
    // a size cap before starting madcawl
    e("logs/madcawl-host.log", External, "madcawl-shell (its output), L1 peerbus (opens, empties past the cap)", "1 MiB", true),
    // madcawl writes its audit itself; no marspot build reads it
    e("logs/madcawl-audit.log", External, "madcawl-shell (started by peerbus)", "madcawl's own", false),
    // services
    e("helpers/events.log", Fact, "helpers (lifetimes read by claudecode allocation_forecast)", "8 MiB, rotated", true),
    e("helpers/events.log.<n>", Fact, "helpers (rotated)", "7 files", true),
    e("helpers/containers/<id>", Draft, "helpers::container::pending", "one per helper", true),
    e("helpers/<id>/task.md", Draft, "helpers", "one per helper", true),
    e("helpers/<id>/run.sh", Draft, "helpers", "one per helper", true),
    e("helpers/<id>/resume.sh", Draft, "helpers::container", "one per helper", true),
    e("helpers/<id>/image", Draft, "helpers::container", "one per helper", true),
    e("helpers/<id>/env", Draft, "helpers::container::login", "one per helper", true),
    e("helpers/<id>/expires", Draft, "helpers::container::login", "one per helper", true),
    e("helpers/<id>/told-dialog", Runtime, "helpers::passing", "marker", true),
    e("helpers/<id>/entered-*", Runtime, "helpers::passing", "marker", true),
    e("helpers/<id>/result.md", External, "the helper agent", "the agent's", true),
    // the agent's own home; marspot reads no file in it
    e("helpers/<id>/**", External, "the helper agent, its container home", "removed with the helper", false),
    e("plugins/claudecode/alloc_events/<date>.tsv", Fact, "claudecode allocation_events (alloc, sched)", "15 days", true),
    // summed from each hour's events once that hour ends, never rewritten,
    // and kept past the events it was summed from
    e("plugins/claudecode/alloc_hours/<n>.tsv", Fact, "claudecode allocation_hours (alloc)", "60 days, one file a month", true),
    e("plugins/codex/alloc_events/<date>.tsv", Fact, "codex alloc (claudecode allocation_events)", "15 days", true),
    e("plugins/gemini/alloc_events/<date>.tsv", Fact, "gemini alloc (claudecode allocation_events)", "15 days", true),
    e("plugins/kimi/alloc_events/<date>.tsv", Fact, "kimi alloc (claudecode allocation_events)", "15 days", true),
    e("plugins/claudecode/refused.tsv", Derived, "claudecode switching (accounts)", "one row per account", true),
    e("plugins/claudecode/heads_up.tsv", Draft, "claudecode switching", "one row per pane", true),
    e("plugins/claudecode/dormant.tsv", Draft, "claudecode dormant", "one row per pane", true),
    e("plugins/claudecode/fable_need.tsv", Draft, "claudecode fable_need", "one row per session", true),
    e("plugins/claudecode/usage_trend.tsv", Derived, "claudecode burn", "one row per account", true),
    e("plugins/claudecode/model/<sid>", Derived, "marspot-shell --cc-statusline (claude's hook)", "3 days", true),
    e("plugins/kimi/refused.tsv", Derived, "kimi balance (accounts)", "one row per account", true),
    e("plugins/kimi/heads_up.tsv", Draft, "kimi balance", "one row per pane", true),
    e("usage/claude-usage.json", Derived, "usage", "one per account", true),
    e("usage/codex-usage.json", Derived, "usage", "one per account", true),
    e("usage/kimi-usage.json", Derived, "usage", "one per account", true),
    e("usage/claude-waits.json", Derived, "usage claude", "one per account", true),
    e("usage/kimi-waits.json", Derived, "usage kimi", "one per account", true),
    e("usage/usage_samples/<date>.tsv", Fact, "usage samples", "15 days", true),
    // the accounts panel (L2) reads it
    e("usage/resets.json", Derived, "usage resets", "one per account", true),
    e("usage/resets/<date>.tsv", Fact, "usage resets", "15 days", true),
    e("usage/reset_claims/<date>.tsv", Fact, "reset claims", "15 days", true),
    e("zombie/told.tsv", Derived, "zombie store", "2048 rows", true),
    e("zombie/clock.tsv", Derived, "zombie store", "one row per process", true),
    e("zombie/told-dryrun.tsv", Derived, "zombie store (dry run)", "2048 rows", true),
    e("zombie/clock-dryrun.tsv", Derived, "zombie store (dry run)", "one row per process", true),
    e("zombie/names.tsv", Derived, "zombie checker", "one row per process", true),
    e("zombie/orphans.tsv", Derived, "zombie checker", "bounded list", true),
    // the next build appends to it and rotates it by size
    e("zombie/dryrun.log", Fact, "zombie dryrun, teller", "1 MiB, rotated", true),
    // only ever renamed over
    e("zombie/dryrun.log.1", Fact, "zombie dryrun (rotated)", "1 MiB", false),
    // written and renamed in one call
    e("zombie/*.tsv.new", Runtime, "zombie store (atomic replace)", "one per file", false),
    e("handoff/<id>.ledger", Draft, "handoff ledger (switch)", "one per pane", true),
    e("handoff/<id>.md", Draft, "handoff ledger (switch)", "one per pane", true),
    e("handoff/<id>.model", Draft, "handoff carried (switch)", "one per pane", true),
    e("peerbus/titles.tsv", Derived, "peerbus title (messaging)", "one row per pane", true),
    // the relay is marspot-peer, another binary
    e("peerbus/<n>-in.sock", Runtime, "peerbus inbox relay", "one per relay", true),
    e("peerbus/<n>-send.sock", Runtime, "peerbus inbox relay", "one per relay", true),
    e("peerbus/gemini/*.jsonl", Fact, "peerbus gemini_bind (mail), marspot-peer hook", "removed once delivered", true),
    e("peerbus/kimi/*.jsonl", Fact, "peerbus gemini_bind (mail), marspot-peer hook", "removed once delivered", true),
    e("claude-mcp.json", Derived, "peerbus claude_mcp (config_apply)", "fixed", true),
    // madcawl-shell listens; the L1 of the next build reconnects to it
    e("madcawl.sock", Runtime, "peerbus madcawl_host_proc", "socket", true),
    e("madcawl-host.pid", Runtime, "peerbus madcawl_host", "one number", true),
    // bound and renamed in one call
    e(".madcawl-bind-<n>/s", Runtime, "peerbus madcawl_host_proc (bind, then rename)", "one socket", false),
    e("agents/gemini.tag", Draft, "agents (gemini install)", "one tag", true),
    e("sched/status.json", Derived, "sched (not written yet)", "fixed", true),
    // the temporary half of every atomic replace
    e("**/*.tmp", Runtime, "the writer of the file it replaces", "one per file", false),
    // the dev sandbox's own files
    // the dev scripts' files are no build's
    e("dev.pids", Runtime, "bin/_dev-sandbox.sh", "one line per process", false),
    e("*.run.log", Runtime, "bin/_dev-sandbox.sh", "one per script run", false),
    // left by older builds, read by none
    e("dev-window-state.bin", Draft, LEFT_BEHIND, "none written", false),
    e("l1-fd-vault.sock", Runtime, LEFT_BEHIND, "none written", false),
    e("bootstrap.log", Fact, LEFT_BEHIND, "none written", false),
    e("binaries/*/marspot-shelld", Draft, LEFT_BEHIND, "none written", false),
    e("previous/**", Draft, LEFT_BEHIND, "none written", false),
    e("rescue-<date>/**", Draft, LEFT_BEHIND, "none written", false),
    e("rescue-*/**", Draft, LEFT_BEHIND, "none written", false),
    e("retired-land-bundle/**", Draft, LEFT_BEHIND, "none written", false),
    e("retired-launchagent-backup/**", Draft, LEFT_BEHIND, "none written", false),
    e("**/.DS_Store", External, "Finder", "one per directory", false),
];

/// The fact rows whose files are consumed: taken whole and removed, and
/// a new file of the same name started later.  The run audit does not
/// hold them to "only appended".
pub(crate) const NOT_APPEND_ONLY: &[&str] = &["peerbus/gemini/*.jsonl", "peerbus/kimi/*.jsonl"];

/// The fact rows rotated by a rename, and the rows of the files their
/// bytes move to (`.gz` ones compressed).  The run audit lets such a
/// file restart only when its earlier bytes now begin one of those; the
/// rotated files themselves are only renamed over, aged out or, for
/// `zombie/dryrun.log.1`, replaced.
pub(crate) const ROTATED: &[(&str, &[&str])] = &[
    ("control-audit.log", &["control-audit.log.<n>"]),
    ("helpers/events.log", &["helpers/events.log.<n>"]),
    ("zombie/dryrun.log", &["zombie/dryrun.log.1"]),
    ("logs/marspot.log", &["logs/marspot.<n>.log", "logs/marspot.<n>.log.gz"]),
];

/// The row of the file at `path`, relative to the state root.
pub(crate) fn lookup(path: &str) -> Option<&'static Entry> {
    STATE_MAP.iter().find(|e| pattern::matches(e.path, path))
}

/// Whether some row puts files under the directory `dir`.  The rows
/// that match at any depth are left out: they would vouch for every
/// directory.
pub(crate) fn has_dir(dir: &str) -> bool {
    STATE_MAP.iter().filter(|e| !e.path.starts_with("**")).any(|e| pattern::is_dir_of(e.path, dir))
}

/// Whether `path` names a registered file or a directory files are
/// registered under.
pub(crate) fn is_registered(path: &str) -> bool {
    lookup(path).is_some_and(|e| !e.path.starts_with("**")) || has_dir(path)
}

/// The rows a path today's source builds may fall under.  Not the
/// catch-alls, which would vouch for anything; not what only older
/// builds left; not a whole tree that belongs to another program (a
/// helper agent's home): a path marspot builds there is a file marspot
/// uses and needs a row of its own.
pub(crate) fn source_rows() -> impl Iterator<Item = &'static Entry> {
    STATE_MAP.iter().filter(|e| !e.path.starts_with("**") && e.writer != LEFT_BEHIND && !(e.kind == External && e.path.ends_with("/**")))
}

/// Whether today's code may write the file `path`: some row of
/// [`source_rows`] is its row.
pub(crate) fn source_may_write(path: &str) -> bool {
    source_rows().any(|e| pattern::matches(e.path, path))
}

/// Whether today's code may put files under the directory `dir`.
pub(crate) fn source_may_write_under(dir: &str) -> bool {
    source_rows().any(|e| pattern::is_dir_of(e.path, dir))
}
