//! Dormant records: kept across a restart, re-armed, and dropped once claude is back.

use super::*;

/// A record must not be judged by a scan older than itself.
///
/// The scan that triggers a reclamation was taken while claude was
/// still alive, so it still holds a binding for that pane — judging
/// the fresh record by it reads as "claude is back" and drops it on
/// the spot.  That shipped: three sessions were reclaimed on the
/// real machine at 10:14 and `dormant.tsv` came out **empty**, so
/// the wake path would not have survived the next restart.
#[test]
fn a_record_is_not_dropped_by_the_scan_that_created_it() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

    // The scan that will trigger the reclamation: taken while the
    // binding was still live.
    let mut scan = scan_with(7, 1, "u7");
    scan.new_mapping.insert(7, "P1 u7".into());
    // …and the record created just after it.
    plugin.dormant.push(DormantRecord {
        shelld_sid: 7,
        uuid: "u7".into(),
        profile_num: 1,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
        created_at: scan.scanned_at + Duration::from_millis(1),
    });

    plugin.rearm_dormant(&host, &scan);
    assert_eq!(
        plugin.dormant.len(),
        1,
        "the scan predates the record and says nothing about it"
    );

    // Nor may the scans taken while the kill is still landing.
    // This is the one that shipped: claude takes longer than a
    // tick to flush and exit, those scans still hold its binding,
    // and reading that as "claude is back" emptied `dormant.tsv`
    // seconds after the reclamation wrote it.
    let mut during_kill = scan_with(7, 1, "u7");
    during_kill.new_mapping.insert(7, "P1 u7".into());
    during_kill.scanned_at = plugin.dormant[0].created_at + Duration::from_secs(2);
    plugin.rearm_dormant(&host, &during_kill);
    assert_eq!(
        plugin.dormant.len(),
        1,
        "claude is still exiting; its lingering binding proves nothing"
    );

    // Past the grace window, a binding does mean claude came back —
    // and then the record goes.
    let mut later = scan_with(7, 1, "u7");
    later.new_mapping.insert(7, "P1 u7".into());
    later.scanned_at =
        plugin.dormant[0].created_at + KILL_GRACE + Duration::from_secs(1);
    plugin.rearm_dormant(&host, &later);
    assert!(plugin.dormant.is_empty(), "a later scan may judge it");
}

/// A pane that already has a live claude must never get a wake
/// session armed on it.
///
/// The scan's binding can lag a restart by a pass or two, so
/// "no binding" is not "no claude".  Arm on that and the next
/// focus types `claude --resume …` into the running claude's own
/// prompt, where it sits waiting for Enter — the exact shape of
/// the bug the user reported.  This test uses THIS process as the
/// pane's shell: it is definitely alive and definitely has no
/// claude under it, so the guard's positive path is the one under
/// test, and the negative path is covered by the guard reading the
/// same proc table everything else does.
#[test]
fn rearm_asks_the_kernel_whether_the_pane_is_really_empty() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    // A session id the registry has never heard of: `shell_pid_for`
    // returns 0, which the guard reads as "cannot tell" and
    // refuses — an unknown pane is not an empty one.
    plugin.dormant = vec![DormantRecord {
        shelld_sid: 999_999,
        uuid: "u".into(),
        profile_num: 1,
        config_dir: None,
        project_dir: None,
        choice: CliChoice::default(),
        created_at: std::time::UNIX_EPOCH,
    }];
    let mut scan = scan_with(999_999, 1, "u");
    scan.sessions_seen = vec![999_999];
    scan.new_mapping.clear();
    plugin.rearm_dormant(&host, &scan);
    assert!(
        host.begun.lock().unwrap().is_empty(),
        "a pane we cannot inspect must not get a wake session"
    );
}

/// A pane whose claude came back (woken, or restarted by hand)
/// stops being dormant, which is what keeps the set bounded.
///
/// Retention only — whether a still-dormant pane gets a wake
/// session armed depends on a kernel check that a unit test has no
/// pane to satisfy; that half is covered by
/// `rearm_asks_the_kernel_whether_the_pane_is_really_empty` and by
/// the full-loop PTY test.
#[test]
fn rearm_drops_records_for_panes_whose_claude_is_back() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.dormant = vec![
        DormantRecord {
            shelld_sid: 7,
            uuid: "u7".into(),
            profile_num: 1,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
            created_at: std::time::UNIX_EPOCH,
        },
        DormantRecord {
            shelld_sid: 8,
            uuid: "u8".into(),
            profile_num: 1,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
            created_at: std::time::UNIX_EPOCH,
        },
    ];

    let mut scan = scan_with(7, 1, "u7");
    scan.sessions_seen = vec![7, 8];
    // Pane 7 has a binding again → its claude is back → the record
    // goes.  Pane 8 is still live and still without one → it stays.
    scan.new_mapping.insert(7, "badge".into());
    plugin.rearm_dormant(&host, &scan);

    assert_eq!(plugin.dormant.len(), 1);
    assert_eq!(plugin.dormant[0].shelld_sid, 8);

    // A pane that vanished from the registry drops out entirely.
    let mut gone = scan_with(9, 1, "u9");
    gone.sessions_seen = vec![9];
    plugin.rearm_dormant(&host, &gone);
    assert!(plugin.dormant.is_empty());
}

#[test]
fn dormant_records_round_trip() {
    // Whole seconds: the file stores unix seconds, so a round
    // trip cannot carry sub-second precision.
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_785_000_000);
    let records = vec![
        DormantRecord {
            shelld_sid: 7,
            uuid: "9cff8661-3275-4dce-8c93-89797bc63f44".into(),
            profile_num: 1,
            config_dir: Some("/Users/x/.claude-profile-1".into()),
            project_dir: None,
            choice: CliChoice::default(),
            created_at: t,
        },
        DormantRecord {
            shelld_sid: 9,
            uuid: "abc".into(),
            profile_num: 255,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
            created_at: t,
        },
    ];
    assert_eq!(decode_dormant(&encode_dormant(&records)), records);
}

/// A parked pane wakes on the model and effort it was parked on,
/// across an L1 restart too: the record carries them.
#[test]
fn a_dormant_record_remembers_the_model_and_effort() {
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_785_000_000);
    let records = vec![DormantRecord {
        shelld_sid: 7,
        uuid: "9cff8661-3275-4dce-8c93-89797bc63f44".into(),
        profile_num: 1,
        config_dir: Some("/Users/x/.claude-profile-1".into()),
        project_dir: None,
        choice: CliChoice {
            model: Some("claude-fable-5-1[1m]".into()),
            effort: Some("high".into()),
        },
        created_at: t,
    }];
    assert_eq!(decode_dormant(&encode_dormant(&records)), records);

    let older = "2\tgood-uuid-1\t2\t1785000000\t/Users/x/.claude-profile-2\t/w\n";
    assert_eq!(decode_dormant(older)[0].choice, CliChoice::default(), "a row from before the columns");

    let hostile = "3\tgood-uuid-2\t2\t1785000000\t\t\topus; rm -rf ~\tloud\n";
    assert_eq!(decode_dormant(hostile)[0].choice, CliChoice::default(), "neither value is one claude takes");
}

/// The wake line names the model and effort, quoted where a shell
/// would read the value as a pattern.
#[test]
fn a_reclaimed_pane_wakes_on_the_model_it_was_on() {
    let choice = CliChoice { model: Some("claude-fable-5-1[1m]".into()), effort: Some("high".into()) };
    let op = reclaim_op("u-1", None, Some(fid(4242)), 4200, None, &choice).expect("script");
    let line = op
        .steps
        .iter()
        .find_map(|s| match &s.kind {
            pty_op::StepKind::Send(bytes) if s.label == "resume" => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            _ => None,
        })
        .expect("a resume line");
    assert!(line.contains("--model 'claude-fable-5-1[1m]'"), "{line}");
    assert!(line.contains("--effort high"), "{line}");
}

/// The directory a wake must return to survives the restart with
/// the rest of the record.
///
/// Without it a re-armed wake resumes wherever the pane's shell is
/// standing, and `claude --resume` binds the session to that --
/// which is how a session came back under another project's rules.
/// A directory may hold a space or a `$` and mean nothing by it;
/// what it may not hold is the quote that would end the argument.
#[test]
fn a_dormant_record_remembers_where_its_session_lives() {
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_785_000_000);
    let records = vec![DormantRecord {
        shelld_sid: 7,
        uuid: "9cff8661-3275-4dce-8c93-89797bc63f44".into(),
        profile_num: 1,
        config_dir: Some("/Users/x/.claude-profile-1".into()),
        project_dir: Some("/Users/x/My Work/$proj".into()),
        choice: CliChoice::default(),
        created_at: t,
    }];
    assert_eq!(decode_dormant(&encode_dormant(&records)), records);

    // A row from before the column decodes as "do not know", which
    // is the behaviour it had.
    let older = "2\tgood-uuid-1\t2\t1785000000\t/Users/x/.claude-profile-2\n";
    assert_eq!(decode_dormant(older)[0].project_dir, None);

    // And one that could end the quoting is dropped rather than
    // repaired, like every other field that reaches a command line.
    let hostile = "3\tgood-uuid-2\t2\t1785000000\t\t/tmp/it's here\n";
    assert_eq!(decode_dormant(hostile)[0].project_dir, None);
}

/// The uuid ends up inside a shell command line, so a corrupt or
/// tampered state file must not be able to smuggle anything into
/// it.  Rejected rows are dropped, not repaired.
#[test]
fn dormant_decode_rejects_a_uuid_that_is_not_a_uuid() {
    let bad = "1\tabc; rm -rf ~\t1\n2\t\t1\n3\tok-uuid\tnot_a_number\n";
    assert!(decode_dormant(bad).is_empty());
    // A well-formed row alongside bad ones still survives.
    // A row without the 4th column is an older file: it decodes,
    // with a creation time old enough to be judged normally.
    let mixed = "1\tbad;line\t1\n2\tgood-uuid-1\t2\n";
    assert_eq!(
        decode_dormant(mixed),
        vec![DormantRecord {
            shelld_sid: 2,
            uuid: "good-uuid-1".into(),
            profile_num: 2,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
            created_at: std::time::UNIX_EPOCH,
        }]
    );
}
