//! The log of a test process, and the lines a machine without devops
//! must never write.  The log sink is process-wide and opened once, so
//! the log is one test's alone only under nextest, which runs each test
//! in a process of its own.  Under plain `cargo test` every test of the
//! binary shares the log under the first test's state root, and a check
//! of it sees the lines of tests running beside it.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Opens the log under the state root the test has set and gives its
/// path; later calls give the same path, as the log stays where it
/// was first opened.
pub(crate) fn open() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = marspot_term::paths::log_dir().join("marspot.log");
        let _ = marspot_term::logx::sink::ensure_open();
        path
    })
    .clone()
}

pub(crate) fn text() -> String {
    std::fs::read_to_string(open()).unwrap_or_default()
}

/// What only a devops on this machine can make marspot write: a read
/// of its files, a feed taken from it, a request to it that failed or
/// was tried again, a reading compared with its own.
const DEVOPS_ONLY: [&str; 8] = [
    ".local/state/devops",
    ".config/devops",
    "devops_feed",
    "devops could not be reached",
    "devops did not confirm",
    "devops answered",
    "usage.resets.agree",
    "usage.resets.disagree",
];

/// The lines of `log` that show marspot depending on devops, or a
/// warning or error that names it.
pub(crate) fn devops_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| {
            let level = l.split('\t').nth(2).unwrap_or_default();
            let lower = l.to_ascii_lowercase();
            DEVOPS_ONLY.iter().any(|s| l.contains(s))
                || l.split('\t').any(|f| f == "by=devops")
                || (matches!(level, "WARN" | "ERROR") && lower.contains("devops"))
                || (lower.contains("devops") && (lower.contains("retry") || lower.contains("timed out") || lower.contains("timeout")))
        })
        .collect()
}

#[test]
fn a_devops_line_is_told_from_a_pane_named_devops() {
    let ok = "t\t1\tINFO\tshell\t1\t2\tcc.bind\tpane bound\tproject=devops\n\
              t\t1\tINFO\tshell\t1\t2\tsvc.reset.claim\tprovider=codex by=devops-ui result=unavailable\n\
              t\t1\tINFO\tshell\t1\t2\tusage.collector\tfeed collector\tprovider=claude\tby=marspot\n";
    assert!(devops_lines(ok).is_empty());
    let bad = "t\t1\tINFO\tshell\t1\t2\tusage.collector\tfeed collector\tprovider=claude\tby=devops\n\
               t\t1\tWARN\tshell\t1\t2\tx\tdevops is gone\n\
               t\t1\tINFO\tshell\t1\t2\tx\tasking devops again: retry 2\n";
    assert_eq!(devops_lines(bad).len(), 3);
}
