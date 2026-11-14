use super::*;

#[path = "reattach_tests.rs"]
mod reattach_tests;
use marspot::state::{SavedPane, SavedWindowLayout};
use marspot_term::session_registry::{
    self as reg, write_session_entry, SessionEntry,
};

/// The test's own state root and session binary, and every L3 it
/// started gone by the time it is dropped.
///
/// Dropped in field order: the L3s are killed and reaped while the
/// registry that names them is still where the variable points,
/// then the variables go back, then the directory goes.
pub(super) struct Sandbox {
    l3s: L3Reaper,
    env: marspot_term::test_env::EnvGuard,
    root: ShortRoot,
}

/// A state root with a short path.  An L3 binds its socket at
/// `<root>/sessions/<id>/sock`, macOS caps a socket path at 104
/// bytes, and the per-user temp dir alone is half of that: under
/// it the L3 dies at `uds_bind_failed` before it registers.
struct ShortRoot(std::path::PathBuf);

impl ShortRoot {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::path::PathBuf::from(format!(
            "/tmp/mspt-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for ShortRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills, on drop, every L3 the sandbox's registry names and every
/// pid handed to [`Sandbox::own`], and waits until each has exited:
/// one still tearing down when the test process exits holds its
/// stdout, and the runner reports the test as leaking.
struct L3Reaper {
    extra: std::cell::RefCell<Vec<i32>>,
}

impl Drop for L3Reaper {
    fn drop(&mut self) {
        let mut pids: Vec<i32> = reg::list_session_entries()
            .into_iter()
            .map(|e| e.pid)
            .chain(self.extra.borrow().iter().copied())
            .filter(|&pid| reg::pid_is_live_session(pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        for &pid in &pids {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        for &pid in &pids {
            // every L3 here is this process's child, so this reaps it
            let mut st = 0;
            unsafe { libc::waitpid(pid, &mut st, 0) };
        }
    }
}

impl Sandbox {
    pub(super) fn new() -> Self {
        let root = ShortRoot::new();
        let mut env = marspot_term::test_env::EnvGuard::new();
        env.set("MARSPOT_STATE_DIR", &root.0)
            .set("MARSPOT_SESSION_BIN", real_session_bin())
            .remove("MARSPOT_SESSION_ID");
        // POSIX shm names (`/msp-s-<id>`) are a GLOBAL namespace:
        // the sandbox isolates the filesystem but not shm.  With
        // every sandbox allocating ids from 1, two tests spawning
        // concurrently raced on `/msp-s-1` — create_region_named
        // unlink+O_EXCL either detached the other test's live
        // region or lost the O_EXCL race, and the spawn `expect`
        // blew up (the suite's one flaky).  Seeding the counter
        // with a per-process base keeps every test's id space —
        // and therefore its shm names — disjoint.  Production is
        // untouched: one state root, one monotonic counter.
        let sessions = root.0.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(".next_id"),
            format!("{}", std::process::id() as u64 * 100_000),
        )
        .unwrap();
        Self {
            l3s: L3Reaper { extra: Default::default() },
            env,
            root,
        }
    }
    fn dir(&self) -> &std::path::Path {
        &self.root.0
    }
    pub(super) fn set(&mut self, key: &str, value: &str) {
        self.env.set(key, value);
    }
    fn break_session_bin(&mut self) {
        self.set("MARSPOT_SESSION_BIN", "/nonexistent/marspot-session-broken");
    }
    /// Kill `pid` with the rest, for an L3 the registry may no
    /// longer name by the time the test ends.
    pub(super) fn own(&self, pid: i32) {
        self.l3s.extra.borrow_mut().push(pid);
    }
}

/// target/<profile>/marspot-session, built beside this test binary.
fn real_session_bin() -> std::path::PathBuf {
    marspot_term::test_env::built_binary("marspot-session")
}

fn dead_entry(id: u64) -> SessionEntry {
    SessionEntry {
        id,
        // Far above macOS pid_max (99999) — kill(pid, 0) fails,
        // guaranteed dead.
        pid: 3_999_999,
        socket: reg::session_socket_path(id),
        cols: 80,
        rows: 24,
        title: format!("saved-title-{id}"),
        cwd: String::new(),
        proto_version: reg::PROTO_VERSION,
        created_at_unix: 1,
        shm_name: String::new(),
        shell_child_pid: 0,
        build: String::new(),
        stamps_regions: false,
    }
}

fn saved(panes: &[(u64, &str)]) -> SavedWindowLayout {
    SavedWindowLayout {
        grid_cols: 3,
        grid_rows: 3,
        focused_idx: 0,
        panes: panes
            .iter()
            .map(|(sid, title)| SavedPane {
                sid: *sid,
                flags: 0,
                custom_title: title.to_string(),
                last_cwd: String::new(),
            })
            .collect(),
    }
}

fn pane_sids(panes: &[Pane]) -> Vec<u64> {
    panes
        .iter()
        .map(|p| p.session().shelld_session_id().unwrap_or(0))
        .collect()
}

/// Full-crash restore (the 2026-07-17 field report): every L3
/// dead, session dirs on disk in arbitrary readdir order.  Slots
/// must come back in SAVED order — not readdir order — with each
/// slot's own sid resurrected as a live pane.
#[test]
fn dead_sessions_resurrect_in_saved_slot_order() {
    let _sb = Sandbox::new();
    for id in [12u64, 5, 9] {
        write_session_entry(&dead_entry(id)).unwrap();
    }
    let s = saved(&[(9, "nine"), (12, "twelve"), (5, "five")]);
    let (tx, _rx) = mpsc::channel();
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 3, 60, 16, &tx, true, &Default::default());
    assert_eq!(
        pane_sids(&panes),
        vec![9, 12, 5],
        "slots must follow saved order, not readdir order"
    );
    assert!(reattached.is_empty(), "nothing was alive to reattach");
    for (i, p) in panes.iter().enumerate() {
        assert!(
            !p.is_vacant(),
            "slot {i} must be a live resurrected L3, got vacant"
        );
    }
}

/// Assembly binds panes to sessions, and nothing else: a pane's
/// name is derived from its directory at render time
/// (`marspot::pane_name`), so there is no title to carry across a
/// boot.  What used to be tested here — saved title beats
/// entry.toml — described a feature that no longer exists.
#[test]
fn assembly_binds_the_saved_slots_to_their_sessions() {
    let _sb = Sandbox::new();
    for id in [4u64, 6] {
        write_session_entry(&dead_entry(id)).unwrap();
    }
    let s = saved(&[(4, ""), (6, "")]);
    let (tx, _rx) = mpsc::channel();
    let (panes, _) = assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
    assert_eq!(pane_sids(&panes), vec![4, 6]);
}

/// Spawn failure must hold the slot open as a vacant pane carrying
/// the sid — never compact, never shift the neighbours.
#[test]
fn spawn_failure_yields_vacant_slots_never_compacts() {
    let mut sb = Sandbox::new();
    sb.break_session_bin();
    let s = saved(&[(7, "seven"), (0, ""), (8, "eight")]);
    let (tx, _rx) = mpsc::channel();
    let (panes, _) =
        assemble_panes_at_boot(Some(&s), 3, 60, 16, &tx, true, &Default::default());
    assert_eq!(panes.len(), 3, "failed slots must NOT compact away");
    let sids = pane_sids(&panes);
    assert_eq!(sids[0], 7, "slot 0 keeps its sid for revive");
    assert_ne!(sids[1], 0, "anonymous slot got a fresh allocated id");
    assert_eq!(sids[2], 8, "slot 2 keeps its sid for revive");
    for (i, p) in panes.iter().enumerate() {
        assert!(p.is_vacant(), "slot {i} must be vacant");
        assert!(p.is_exited(), "vacant reports exited → revive path");
    }
}

/// A live session missing from the saved layout is real user work
/// — it must be ADOPTED as an extra pane, not SIGKILLed + deleted
/// (the pre-RFC-004 behaviour when a layout shrank).
#[test]
fn orphan_live_session_adopted_not_killed() {
    let _sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    // Bring up a REAL live L3 outside any saved layout.
    let orphan_sid = reg::allocate_next_session_id().unwrap();
    let live = spawn_l3_pane_with_cwd(60, 16, orphan_sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    // Keep it alive across the assembly; the sandbox kills it via
    // the registry at test end.
    std::mem::forget(live);

    let s = saved(&[(0, "")]);
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &Default::default());
    let sids = pane_sids(&panes);
    assert_eq!(panes.len(), 2, "slot pane + adopted orphan: {sids:?}");
    assert!(
        sids.contains(&orphan_sid),
        "live orphan {orphan_sid} must be adopted, got {sids:?}"
    );
    assert_eq!(reattached, vec![orphan_sid]);
    // And its dir must still exist (never deleted).
    assert!(reg::session_dir(orphan_sid).exists());
}

/// A dead dir that no slot references retires to the recycle bin
/// (recoverable), never deletes.
#[test]
fn unreferenced_dead_dir_retires_to_recycle_bin() {
    let mut sb = Sandbox::new();
    write_session_entry(&dead_entry(31)).unwrap();
    // Give it recognisable history so the retired copy is provably
    // the same dir.
    std::fs::write(reg::session_dir(31).join("bytelog"), b"HISTORY").unwrap();
    sb.break_session_bin(); // slots don't matter; keep them vacant
    let s = saved(&[(700, "")]);
    let (tx, _rx) = mpsc::channel();
    let (panes, _) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &Default::default());
    assert_eq!(panes.len(), 1);
    assert!(
        !reg::session_dir(31).exists(),
        "unreferenced dead dir must move out of sessions/"
    );
    let retired_root = sb.dir().join("retired");
    let retired: Vec<_> = std::fs::read_dir(&retired_root)
        .expect("retired/ must exist")
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("31-")
        })
        .collect();
    assert_eq!(retired.len(), 1, "dir 31 must be in the recycle bin");
    let bytelog = retired[0].path().join("bytelog");
    assert_eq!(std::fs::read(bytelog).unwrap(), b"HISTORY");
}


/// RFC-005 step 6b — a restored window runs the same assembly, but
/// it must NOT sweep the registry.  It knows only its own saved
/// sids, so sweeping would (a) adopt another window's live panes a
/// second time and (b) retire the dirs of sessions it never heard
/// of.  Both halves are checked here against a sandbox that
/// contains exactly one of each.
#[test]
fn a_restore_assembly_leaves_other_windows_sessions_alone() {
    let mut sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    // A live session belonging to some other window.
    let other_sid = reg::allocate_next_session_id().unwrap();
    let live = spawn_l3_pane_with_cwd(60, 16, other_sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    std::mem::forget(live); // the sandbox kills it via the registry
    // A dead dir the sweeping assembly would retire.
    write_session_entry(&dead_entry(31)).unwrap();
    std::fs::write(reg::session_dir(31).join("bytelog"), b"HISTORY").unwrap();

    sb.break_session_bin(); // this window's own slot stays vacant
    let s = saved(&[(700, "")]);
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, false, &Default::default());

    let sids = pane_sids(&panes);
    assert_eq!(panes.len(), 1, "only its own slot: {sids:?}");
    assert!(
        !sids.contains(&other_sid),
        "another window's live session must not be adopted: {sids:?}"
    );
    assert!(reattached.is_empty());
    assert!(
        reg::session_dir(31).exists(),
        "a non-sweeping assembly must not retire dirs it knows nothing about"
    );
    assert!(reg::session_dir(other_sid).exists());
}

/// A session another window's saved record owns is neither an
/// orphan to adopt nor junk to retire — its window just has not
/// been opened yet.
///
/// This is the core-swap case: the replacement core assembles the
/// boot window first, while windows 2..N are still unannounced and
/// their L3s very much alive.  Without the reservation the boot
/// window swallowed them, and the real window's restore then tried
/// to reattach sessions that were already bound elsewhere.
#[test]
fn sessions_owned_by_another_saved_window_are_left_for_it() {
    let mut sb = Sandbox::new();
    let (tx, _rx) = mpsc::channel();
    // Window 2's live session…
    let live_sid = reg::allocate_next_session_id().unwrap();
    let live = spawn_l3_pane_with_cwd(60, 16, live_sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    std::mem::forget(live);
    // …and window 2's dead-but-saved session, which must keep its
    // dir so that window can resurrect it.
    write_session_entry(&dead_entry(41)).unwrap();

    sb.break_session_bin();
    let reserved: std::collections::HashSet<u64> = [live_sid, 41].into_iter().collect();
    let s = saved(&[(700, "")]);
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &reserved);

    let sids = pane_sids(&panes);
    assert_eq!(panes.len(), 1, "boot window keeps only its own slot: {sids:?}");
    assert!(!sids.contains(&live_sid), "reserved live session adopted");
    assert!(reattached.is_empty());
    assert!(
        reg::session_dir(41).exists(),
        "reserved dead session's history must survive for its window"
    );
}

/// 2026-07-28 incident — the machine died with 176 live session
/// processes.  The population cap turns "grow until the OS falls
/// over" into "one spawn refuses, loudly".  Cap lowered via env so
/// proving the refusal doesn't itself need a process storm.
#[test]
fn spawning_past_the_session_population_cap_refuses() {
    let mut sb = Sandbox::new();
    sb.set("MARSPOT_SESSION_CAP", "1");
    let (tx, _rx) = mpsc::channel();
    let first_sid = reg::allocate_next_session_id().unwrap();
    let _first = spawn_l3_pane_with_cwd(60, 16, first_sid, "", &tx)
        .expect("first spawn fits under the cap");
    let second_sid = reg::allocate_next_session_id().unwrap();
    let err = match spawn_l3_pane_with_cwd(60, 16, second_sid, "", &tx) {
        Err(e) => e,
        Ok(_) => panic!("second spawn must refuse at cap 1"),
    };
    assert!(
        err.to_string().contains("hard cap"),
        "refusal must name the cap, got: {err}"
    );
}

/// RFC-006 — a dormant slot restores AS a dormant slot: boot
/// assembly must not reattach, resurrect, or spawn for it.
#[test]
fn a_dormant_slot_boots_as_a_placeholder() {
    let mut sb = Sandbox::new();
    sb.break_session_bin(); // any spawn attempt would fail loudly
    let s = SavedWindowLayout {
        grid_cols: 2,
        grid_rows: 1,
        focused_idx: 0,
        panes: vec![
            SavedPane { sid: 700, ..Default::default() },
            SavedPane {
                flags: marspot::state::PANE_FLAG_DORMANT,
                ..Default::default()
            },
        ],
    };
    let (tx, _rx) = mpsc::channel();
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
    assert_eq!(panes.len(), 2);
    assert!(!panes[0].is_dormant(), "live slot stays a session slot");
    assert!(panes[1].is_dormant(), "dormant slot is a placeholder again");
    assert!(reattached.is_empty());
}

/// Duplicate sid in a corrupt saved state must not double-bind one
/// session to two panes — the second slot falls back to a fresh id.
#[test]
fn duplicate_saved_sid_does_not_double_bind() {
    let mut sb = Sandbox::new();
    sb.break_session_bin();
    let s = saved(&[(9, "a"), (9, "b")]);
    let (tx, _rx) = mpsc::channel();
    let (panes, _) =
        assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
    let sids = pane_sids(&panes);
    assert_eq!(sids.len(), 2);
    assert_eq!(sids[0], 9);
    assert_ne!(sids[1], 9, "second slot must not re-bind sid 9");
}
