//! Assembling the panes of the first window at boot from saved slots and live sessions.

use super::*;

/// RFC-004 B.1 — identity-keyed boot assembly.  Extracted from
/// `main()` so the slot semantics (order preserved, never
/// compacted, sid-stable, vacant on failure) are testable with a
/// sandbox state dir + real L3 binaries (`MARSPOT_SESSION_BIN`).
/// Returns the assembled panes (one per slot + adopted orphans)
/// and the reattached session ids (execv fanout drives off them).
pub(super) fn assemble_panes_at_boot(
    saved_state: Option<&marspot::state::SavedWindowLayout>,
    n_sessions: usize,
    boot_cols: u16,
    boot_rows: u16,
    event_tx: &Sender<CoreEvent>,
    sweeps_registry: bool,
    reserved_sids: &std::collections::HashSet<u64>,
) -> (Vec<Pane>, Vec<u64>) {
    // entry.toml titles, collected during the registry scan below.
    // Last resort of the title fallback chain at the end of assembly.
    let mut session_titles: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();
    let mut panes: Vec<Pane> = Vec::with_capacity(n_sessions);
    // RFC-004 B.1 — identity-keyed slot assembly.  Every saved
    // slot is processed IN ORDER and ALWAYS yields a pane:
    //
    //   sid alive (identity-verified) → reattach; reattach failure
    //     downgrades to resurrection (never delete)
    //   sid dead + dir on disk       → resurrect same id (L3 cold
    //     boot applies state.bin + reopens scrollback.bin)
    //   sid without dir / sid == 0   → fresh spawn (same sid when
    //     the slot names one, so identity survives a lost dir)
    //   any spawn failure            → vacant placeholder pane
    //     carrying the sid (revive-on-keystroke retries it)
    //
    // The old assembly compacted failed slots away (panes shifted,
    // titles/cwd mis-bound by index, and the first
    // save_session_state locked the damage in) and resurrected
    // dead ids in readdir order (错位 after a full crash).
    let raw_list = list_session_entries();
    for e in &raw_list {
        if !e.title.is_empty() {
            session_titles.insert(e.id, e.title.clone());
        }
    }
    let entry_by_id: std::collections::HashMap<
        u64,
        &marspot_term::session_registry::SessionEntry,
    > = raw_list.iter().map(|e| (e.id, e)).collect();
    // Liveness = the pid runs a marspot-session image that started
    // before its entry was written: a recycled pid cannot impersonate
    // a live session, nor eat a SIGKILL meant for one.
    let alive_ids: std::collections::HashSet<u64> = raw_list
        .iter()
        .filter(|e| session_registry::entry_is_live(e))
        .map(|e| e.id)
        .collect();
    let dir_ids: std::collections::HashSet<u64> =
        raw_list.iter().map(|e| e.id).collect();

    // Slot specs come from the saved layout; without one, adopt
    // whatever the registry holds (numeric order), padded with
    // anonymous slots up to the grid size.
    struct SlotSpec {
        sid: u64,
        cwd: String,
        /// RFC-006 — restore as a dormant placeholder: no reattach,
        /// no resurrect, no spawn.  A placeholder is layout.
        dormant: bool,
    }
    let slot_specs: Vec<SlotSpec> = match saved_state {
        Some(s) => s
            .panes
            .iter()
            .take(n_sessions)
            .map(|p| SlotSpec {
                sid: p.sid,
                cwd: p.last_cwd.clone(),
                dormant: p.flags & marspot::state::PANE_FLAG_DORMANT != 0,
            })
            .collect(),
        None => {
            let mut ids: Vec<u64> = dir_ids.iter().copied().collect();
            ids.sort();
            ids.truncate(n_sessions);
            let mut specs: Vec<SlotSpec> = ids
                .into_iter()
                .map(|sid| SlotSpec { sid, cwd: String::new(), dormant: false })
                .collect();
            while specs.len() < n_sessions {
                specs.push(SlotSpec { sid: 0, cwd: String::new(), dormant: false });
            }
            specs
        }
    };

    let mut claimed: std::collections::HashSet<u64> =
        std::collections::HashSet::new();
    let mut reattached_ids: Vec<u64> = Vec::new();
    // Fresh-id allocation that can't collide with a claimed sid:
    // the allocator floors at max(existing dir id) (A.1), but a
    // claimed sid whose dir hasn't been recreated yet is invisible
    // to that floor — skip by hand.  64 tries is unreachable in
    // practice (each miss burns one monotonic id).
    let allocate_fresh = |claimed: &std::collections::HashSet<u64>| -> Option<u64> {
        for _ in 0..64 {
            match allocate_next_session_id() {
                Ok(id) if !claimed.contains(&id) => return Some(id),
                Ok(_) => continue,
                Err(e) => {
                    lx_error!(
                        "core.session_registry.allocate_failed",
                        &format!("{e}")
                    );
                    return None;
                }
            }
        }
        None
    };

    for spec in &slot_specs {
        // RFC-006 — a dormant slot restores AS a dormant slot: it is
        // layout, not a session, and none of the session machinery
        // below (reattach / resurrect / spawn) applies.
        if spec.dormant {
            panes.push(Pane::new_dormant(boot_cols, boot_rows));
            continue;
        }
        let mut sid = spec.sid;
        if sid != 0 && claimed.contains(&sid) {
            // Duplicate sid in saved state (corrupt / hand-edited)
            // — assemble as an anonymous slot instead of
            // double-binding one session to two panes.
            sid = 0;
        }
        // 1) Live session → reattach in place.
        if sid != 0 && alive_ids.contains(&sid) {
            match reattach_l3_pane(sid, event_tx) {
                Ok(pane) => {
                    panes.push(pane);
                    claimed.insert(sid);
                    reattached_ids.push(sid);
                    continue;
                }
                Err(e) => {
                    // Live but it does not answer, or it cannot be
                    // asked for a region that can be shown (see
                    // `reattach_l3_pane`).  The process is verified
                    // to be this session's (A.2), the dir is kept and
                    // the session is started again in place.
                    lx_warn!(
                        "core.reattach.l3_failed",
                        &format!("{e} — SIGKILL verified L3, resurrect in place"),
                        session = sid
                    );
                    if let Some(entry) = entry_by_id.get(&sid) {
                        let _ = marspot::session_signal::signal_entry(entry, libc::SIGKILL);
                    }
                }
            }
        }
        // 2) Dead (or just-killed) with a dir → resurrect same id.
        // 3) No dir → fresh spawn, keeping the slot's sid.
        let spawn_sid = if sid != 0 {
            sid
        } else {
            match allocate_fresh(&claimed) {
                Some(id) => id,
                None => {
                    panes.push(Pane::new_vacant(0, boot_cols, boot_rows));
                    continue;
                }
            }
        };
        // Stale shm from a previous life can't be reattached —
        // tear it down so the fresh L3 publishes a new region.
        if let Some(entry) = entry_by_id.get(&spawn_sid)
            && !entry.shm_name.is_empty()
                && let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                    grid_shm::delete_session_regions(&c);
                }
        // RFC-004 C.2 belt+braces — a resurrect dir still holds the
        // DEAD process's entry.toml; `wait_for_entry` would read it
        // instantly and start connecting before the fresh child has
        // even bound.  Unlink the stale entry + sock (KEEPING
        // scrollback/bytelog/state.bin) so the post-spawn wait
        // synchronises on the fresh child's own registry write.
        // 2026-07-28 — safe-mode boot (crash-loop brake, set by the
        // shell): reattach is fine, fresh processes are not.  The slot
        // stays vacant; the user revives it with a keystroke once the
        // loop is understood.  This is the line that stops a crash
        // loop from spawning a new generation every cycle.
        if std::env::var("MARSPOT_SAFE_MODE").is_ok() {
            lx_warn!(
                "core.boot.safe_mode_vacant",
                "safe mode: slot left vacant instead of spawning fresh",
                session = spawn_sid
            );
            claimed.insert(spawn_sid);
            panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            continue;
        }
        // …but only once we know nobody is living there.  The two
        // unlinks below are what make a session reachable, and doing
        // them under a running L3 strands it for good: its listener fd
        // stays bound to a path that no longer exists, so no future
        // core can dial it and its own deadman (which only watched
        // entry.toml) saw nothing wrong.  Seven sessions were found in
        // exactly that state on 2026-07-29 — an L3 mid-execv had not
        // yet rewritten entry.toml when this boot scanned the
        // registry, so it never made `alive_ids`, and we cleaned the
        // slot out from under a live process.
        //
        // The A.3 dir lock is the right authority here precisely
        // because it does not depend on the registry being current: it
        // is held for the owner's whole life, survives execv, and
        // releases only on process death.  `alive_ids` answers "was
        // there a valid entry when we scanned"; this answers "is
        // anyone home right now".
        let dir_is_occupied = match marspot_term::session_registry::try_lock_session_dir(spawn_sid)
        {
            // Free — drop it straight away so the L3 we spawn takes
            // ownership itself.
            Ok(lock) => {
                std::mem::drop(lock);
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            // Couldn't even attempt the lock (IO error).  Don't let a
            // lock problem block session bootstrap — fall through to
            // the old behaviour.
            Err(_) => false,
        };
        if dir_is_occupied {
            lx_warn!(
                "core.boot.slot_occupied",
                "another process still owns this session dir — leaving the slot vacant \
                 instead of unlinking a live L3's socket",
                session = spawn_sid
            );
            claimed.insert(spawn_sid);
            panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            continue;
        }
        let _ = std::fs::remove_file(
            marspot_term::session_registry::session_entry_path(spawn_sid),
        );
        marspot_term::session_registry::cleanup_stale_socket(spawn_sid);
        match spawn_l3_pane_with_cwd(
            boot_cols, boot_rows, spawn_sid, &spec.cwd, event_tx,
        ) {
            Ok(pane) => {
                panes.push(pane);
                claimed.insert(spawn_sid);
            }
            Err(e) => {
                lx_error!(
                    "core.spawn.l3_boot_failed",
                    &format!("{e} — slot held vacant, revive retries this sid"),
                    session = spawn_sid
                );
                claimed.insert(spawn_sid);
                panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            }
        }
    }

    // Live sessions no slot claimed = real user work whose slot
    // record was lost — append as extra panes rather than kill
    // (the old path SIGKILLed + deleted these when the layout
    // shrank).  Over the hard cap: stop the process but RETIRE
    // the dir (recoverable) instead of deleting.
    //
    // Only the sweeping assembly does this.  RFC-005 step 6b restores
    // the other windows through this same function, and a restored
    // window knows only its own saved sids: were it to sweep, it would
    // adopt the boot window's panes a second time and retire the dirs
    // of every session it simply never heard of.
    for id in raw_list.iter().map(|e| e.id).filter(|_| sweeps_registry) {
        if !alive_ids.contains(&id) || claimed.contains(&id) {
            continue;
        }
        // Spoken for by ANOTHER window's saved record.  Without this,
        // a core swap with two windows open had the boot window adopt
        // the second window's live panes as orphans — and then the
        // second window's own restore tried to reattach the very same
        // sessions.  They are not orphans; their window just has not
        // been announced yet.
        if reserved_sids.contains(&id) {
            continue;
        }

        if panes.len() >= SESSION_COUNT_HARD_CAP {
            if let Some(entry) = entry_by_id.get(&id) {
                let _ = marspot::session_signal::signal_entry(entry, libc::SIGKILL);
                if !entry.shm_name.is_empty()
                    && let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                        grid_shm::delete_session_regions(&c);
                    }
            }
            match session_registry::retire_session_dir(id) {
                Ok(dst) => lx_warn!(
                    "core.boot.surplus_retired",
                    "over-cap live session stopped + dir retired",
                    session = id,
                    retired_to = dst.display()
                ),
                Err(e) => lx_warn!(
                    "core.boot.surplus_retire_failed",
                    &format!("{e}"),
                    session = id
                ),
            }
            continue;
        }
        match reattach_l3_pane(id, event_tx) {
            Ok(pane) => {
                lx_event!(
                    "core.boot.orphan_adopted",
                    "live session outside saved layout appended as extra pane",
                    session = id
                );
                panes.push(pane);
                claimed.insert(id);
                reattached_ids.push(id);
            }
            Err(e) => lx_warn!(
                "core.reattach.orphan_failed",
                &format!("{e} — left on disk for next boot"),
                session = id
            ),
        }
    }

    // RFC-004 B.4 — recycle-bin GC.  Any session dir that is
    // neither claimed by a slot nor alive moves to retired/
    // (kept RETIRED_TTL_SECS = 14 days), and expired retired
    // entries purge.  Scans dir names directly (not raw_list) so
    // dirs with corrupt entry.toml — invisible to discovery —
    // stop accumulating too.  delete_session (真删) stays
    // reserved for the user's explicit pane close.
    let mut retired = 0usize;
    for id in session_registry::list_session_dir_ids() {
        // Same reason as the orphan loop above: a restored window's
        // `claimed` set covers only its own saved sids, so sweeping
        // here would retire the live dirs of every other window.  And
        // a dead session another window intends to resurrect must keep
        // its dir — retiring it would destroy that pane's history one
        // moment before its window asked for it back.
        if !sweeps_registry
            || claimed.contains(&id)
            || alive_ids.contains(&id)
            || reserved_sids.contains(&id)
        {
            continue;
        }
        if let Some(entry) = entry_by_id.get(&id)
            && !entry.shm_name.is_empty()
                && let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                    grid_shm::delete_session_regions(&c);
                }
        match session_registry::retire_session_dir(id) {
            Ok(_) => retired += 1,
            Err(e) => lx_warn!(
                "core.boot.retire_failed",
                &format!("{e}"),
                session = id
            ),
        }
    }
    let purged = if sweeps_registry {
        session_registry::purge_expired_retired()
    } else {
        0
    };
    lx_event!(
        "core.session_registry.inventory",
        "RFC-004 identity-keyed boot assembly",
        total = raw_list.len(),
        alive = alive_ids.len(),
        reattached = reattached_ids.len(),
        slots = slot_specs.len(),
        panes = panes.len(),
        retired = retired,
        purged_expired = purged
    );

    // Per-pane custom titles.  F3+6 — `saved_state.panes` wins over
    // `session_titles` (the entry.toml-derived source) because the
    // bin file reflects the user's last interactive state, including
    // titles set after the last L3 reattach hop.
    //
    // RFC-004 B.3 — titles bind by IDENTITY, not index: the
    // positional entry is only trusted when its recorded sid matches
    // A pane's name is derived from its directory (`pane_name`), so
    // there is nothing here to restore: what used to be carried across
    // a swap was the user-set title, and there is no such thing now.
    (panes, reattached_ids)
}

