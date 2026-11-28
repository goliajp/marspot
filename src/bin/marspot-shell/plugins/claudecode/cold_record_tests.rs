use super::*;

fn shell(pid: i32) -> pidtree::ProcId {
    pidtree::ProcId { pid, start_us: 1_000 + pid as u64 }
}

/// A shell that started `ago` before `now`.
fn shell_at(pid: i32, now: SystemTime, ago: Duration) -> pidtree::ProcId {
    let us = (now - ago).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as u64;
    pidtree::ProcId { pid, start_us: us }
}

fn row(sid: u64, sh: i32) -> LastBound {
    LastBound {
        sid,
        uuid: format!("5860b61a-0000-4000-8000-{sid:012}"),
        profile: 1,
        config_dir: Some("/Users/x/.claude-profile-1".into()),
        project_dir: Some("/w/my proj".into()),
        choice: CliChoice { model: Some("claude-fable-5-1[1m]".into()), effort: Some("high".into()) },
        shell: shell(sh),
    }
}

#[test]
fn rows_round_trip_and_bad_fields_are_dropped() {
    let rows = vec![row(483, 10), LastBound { config_dir: None, project_dir: None, choice: CliChoice::default(), ..row(7, 11) }];
    assert_eq!(decode(&encode(&rows)), rows);
    let bad = encode(&[LastBound { uuid: "a;rm -rf".into(), ..row(1, 1) }, LastBound { config_dir: Some("/x$(y)".into()), ..row(2, 2) }]);
    assert!(decode(&bad).is_empty(), "a field that could not go on a command line drops the row");
    assert!(decode("1\t2\n").is_empty());
}

/// One session is one pane's: of two rows naming it, the later stays.
#[test]
fn a_session_named_by_two_rows_is_kept_once() {
    let first = row(1, 10);
    let second = LastBound { uuid: first.uuid.clone(), ..row(2, 20) };
    let rows = decode(&encode(&[first, second.clone(), row(3, 30)]));
    assert_eq!(rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![2, 3]);
    let mut rows = vec![row(1, 10), row(3, 30)];
    let moved = LastBound { uuid: row(1, 10).uuid, ..row(4, 40) };
    assert!(upsert(&mut rows, moved));
    assert_eq!(rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![3, 4], "the session moved to pane 4");
}

/// A new binding replaces the pane's row; a model the hook has not
/// recorded yet does not wipe the one already known for the session.
#[test]
fn upsert_keeps_a_known_model_for_the_same_session() {
    let mut rows = vec![row(483, 10)];
    let unknown = LastBound { choice: CliChoice::default(), ..row(483, 10) };
    assert!(!upsert(&mut rows, unknown), "nothing changed");
    assert_eq!(rows[0].choice.model.as_deref(), Some("claude-fable-5-1[1m]"));
    let other = LastBound { uuid: "u-2".into(), choice: CliChoice::default(), ..row(483, 10) };
    assert!(upsert(&mut rows, other));
    assert_eq!(rows[0].choice, CliChoice::default(), "another session's model is not carried over");
    for sid in 0..ROWS_CAP as u64 + 5 {
        upsert(&mut rows, row(1000 + sid, 1));
    }
    assert_eq!(rows.len(), ROWS_CAP);
}

struct World {
    listed: Vec<u64>,
    shells: Vec<(u64, pidtree::ProcId)>,
    typed: Vec<(u64, SystemTime)>,
}

fn forget_in(rows: &mut Vec<LastBound>, gone: &mut HashMap<u64, Instant>, w: &World, now: Instant, wall: SystemTime) -> Vec<u64> {
    let shell = |sid: u64| w.shells.iter().find(|(s, _)| *s == sid).map(|e| e.1);
    let typed = |sid: u64| w.typed.iter().find(|(s, _)| *s == sid).map(|e| e.1);
    let u = Unbound { bound: &|sid| sid == 3, listed: &|sid| w.listed.contains(&sid), shell: &shell, last_input: &typed, now, wall };
    forget(rows, gone, &u)
}

/// A shell that is still the one claude ran under, without claude, is
/// the person having left claude: its row goes after the grace.  A
/// pane whose registry entry is gone was closed.
#[test]
fn rows_go_when_claude_left_the_same_shell_or_the_pane_was_closed() {
    let (t0, wall) = (Instant::now(), SystemTime::now());
    let mut rows = vec![row(1, 10), row(2, 20), row(3, 30), row(4, 40)];
    let mut gone = HashMap::new();
    let w = World {
        listed: vec![1, 2, 3],
        shells: vec![(1, shell(10)), (2, shell_at(99, wall, Duration::from_secs(10))), (3, shell(30))],
        typed: vec![],
    };
    assert_eq!(forget_in(&mut rows, &mut gone, &w, t0, wall), vec![4], "pane 4 has no registry entry");
    let later = t0 + GONE_GRACE + Duration::from_secs(1);
    assert_eq!(forget_in(&mut rows, &mut gone, &w, later, wall), vec![1]);
    assert_eq!(rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![2, 3], "the new shell's row and the bound pane's stay");
}

/// At shutdown the session processes end before this one.  A pane
/// whose process is gone but whose registry entry stays is not closed:
/// its row is kept however long this process has run, for the restart.
#[test]
fn a_session_process_gone_before_the_shell_keeps_its_row() {
    let (t0, wall) = (Instant::now(), SystemTime::now());
    let mut rows = vec![row(1, 10)];
    let mut gone = HashMap::new();
    let w = World { listed: vec![1], shells: vec![], typed: vec![] };
    for later in [t0, t0 + Duration::from_secs(3600), t0 + Duration::from_secs(86_400)] {
        assert!(forget_in(&mut rows, &mut gone, &w, later, wall + later.duration_since(t0)).is_empty());
    }
    assert_eq!(rows.len(), 1);
}

/// A new shell is owed its claude only within the window, and only
/// while nobody typed into the pane since it started.
#[test]
fn a_new_shell_past_the_window_or_typed_into_drops_its_row() {
    let (t0, wall) = (Instant::now(), SystemTime::now());
    let young = shell_at(11, wall, Duration::from_secs(30));
    let old = shell_at(12, wall, WINDOW + Duration::from_secs(1));
    let typed = shell_at(13, wall, Duration::from_secs(30));
    let mut rows = vec![row(1, 10), row(2, 20), row(4, 40)];
    let w = World {
        listed: vec![1, 2, 4],
        shells: vec![(1, young), (2, old), (4, typed)],
        typed: vec![(1, wall - Duration::from_secs(60)), (4, wall - Duration::from_secs(5))],
    };
    assert_eq!(forget_in(&mut rows, &mut HashMap::new(), &w, t0, wall), vec![2, 4]);
    assert_eq!(rows[0].sid, 1, "input before the shell started does not count");
}

#[test]
fn only_a_new_idle_untouched_shell_in_its_window_is_owed_a_restore() {
    let now = SystemTime::now();
    let r = row(1, 10);
    let new = shell_at(11, now, Duration::from_secs(20));
    let pane = |shell, idle_shell, last_input| PaneNow { shell, idle_shell, last_input };
    assert!(owed(&r, Some(&pane(new, true, None)), now));
    assert!(owed(&r, Some(&pane(new, true, Some(now - Duration::from_secs(60)))), now));
    assert!(!owed(&r, Some(&pane(new, true, Some(now - Duration::from_secs(1)))), now), "typed into since");
    assert!(!owed(&r, Some(&pane(shell_at(11, now, WINDOW), true, None)), now), "past the window");
    assert!(!owed(&r, Some(&pane(shell(10), true, None)), now), "the shell claude left");
    assert!(!owed(&r, Some(&pane(new, false, None)), now), "a program is in front");
    assert!(!owed(&r, None, now));
}
