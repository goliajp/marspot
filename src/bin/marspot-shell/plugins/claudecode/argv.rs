//! The session a claude process names in its own argv.

use crate::observe::Proc;

/// The live session uuid as stated by the claude process tree's own
/// argv, if it states one.
///
/// This is the only *authoritative* binding available: everything else
/// is inference from file mtimes.  Two argv shapes carry it —
///
/// * `--session-id <uuid>` — the session this process writes.  Wins,
///   because a forked resume (`--resume old.jsonl --fork-session
///   --session-id new`) writes `new` while naming `old`.
/// * `--resume <uuid>` / `--resume <path>/<uuid>.jsonl` — a plain
///   resume continues writing the file it names.
///
/// Searched on the matched process and its own subtree: the daemon
/// shape (`claude daemon run` → pty host → version binary) puts the
/// flags several levels below the `claude` the pane sees.  A plain
/// interactive `claude` states nothing, which is why the inference path
/// still has to exist.
///
/// `pane_tree` is the pane's descendant list as the observer read it,
/// argv included, so this reads nothing from the kernel.
pub(super) fn argv_session_uuid(claude: &Proc, pane_tree: &[Proc]) -> Option<String> {
    let mut subtree: Vec<&Proc> = vec![claude];
    // Transitive closure by repeated passes.  The claude subtree is a
    // handful of processes and at most a few levels deep, so this beats
    // building a map for it.
    loop {
        let before = subtree.len();
        for p in pane_tree {
            let has = |pid: i32| subtree.iter().any(|s| s.row.pid == pid);
            if has(p.row.ppid) && !has(p.row.pid) {
                subtree.push(p);
            }
        }
        if subtree.len() == before {
            break;
        }
    }
    let mut resumed: Option<String> = None;
    for p in subtree {
        let Some(line) = p.cmdline() else { continue };
        if let Some(u) = flag_uuid(&line, "--session-id") {
            return Some(u);
        }
        if resumed.is_none() {
            resumed = flag_uuid(&line, "--resume");
        }
    }
    resumed
}

/// Value of `flag` in a space-joined argv, reduced to a bare uuid.
/// Accepts both `<uuid>` and `<path>/<uuid>.jsonl`; returns None when
/// the flag is absent or its value isn't uuid-shaped (`--resume` also
/// takes a session *name*).
pub(super) fn flag_uuid(cmdline: &str, flag: &str) -> Option<String> {
    let mut it = cmdline.split(' ');
    let raw = loop {
        let tok = it.next()?;
        if tok == flag {
            break it.next()?;
        }
    };
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".jsonl").unwrap_or(base);
    is_uuid(base).then(|| base.to_string())
}

/// 8-4-4-4-12 lowercase hex with dashes.  Deliberately strict: a
/// non-uuid `--resume` value must fall through to the inference path,
/// not become a bogus badge.
pub(super) fn is_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in GROUPS {
        let Some(p) = parts.next() else { return false };
        if p.len() != want || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn proc(pid: i32, ppid: i32, argv: &str) -> Proc {
        let row = crate::observe::tests::row(pid, ppid, "x");
        let argv: Vec<String> = argv.split(' ').map(str::to_string).collect();
        Proc { row, argv: Some(Arc::from(argv)) }
    }

    /// The daemon shape: the flags sit two levels below the `claude`
    /// the pane runs, and a sibling outside claude's subtree is ignored.
    #[test]
    fn the_uuid_is_read_from_claudes_own_subtree() {
        let id = "9e304c9a-93d3-428d-a6a6-fc537db59dc7";
        let other = "aaaaaaaa-93d3-428d-a6a6-fc537db59dc7";
        let tree = vec![
            proc(10, 5, "claude daemon run"),
            proc(11, 10, "pty-host"),
            proc(12, 11, &format!("2.1.0 --resume {id}")),
            proc(13, 5, &format!("claude --session-id {other}")),
        ];
        assert_eq!(argv_session_uuid(&tree[0], &tree).as_deref(), Some(id));
        assert_eq!(argv_session_uuid(&tree[3], &tree).as_deref(), Some(other));
        assert_eq!(argv_session_uuid(&tree[1], &tree[..2]), None);
    }
}
