use super::*;

const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

fn thread(n: u64) -> String {
    format!("01a133f4-8e7b-7331-9630-{n:012}")
}

fn row(sid: u64, sh: i32) -> LastThread {
    LastThread {
        sid,
        thread: thread(sid),
        home: "/Users/x/.codex-profile-2".into(),
        cwd: Some("/w/my proj".into()),
        model: Some("gpt-5.5".into()),
        shell: pidtree::ProcId { pid: sh, start_us: 1_000 + sh as u64 },
    }
}

#[test]
fn rows_round_trip_and_bad_fields_are_dropped() {
    let rows = vec![row(483, 10), LastThread { cwd: None, model: None, ..row(7, 11) }];
    assert_eq!(decode(&encode(&rows)), rows);
    let bad = encode(&[
        LastThread { thread: "a;rm -rf ~".into(), ..row(1, 1) },
        LastThread { home: "/x$(y)".into(), ..row(2, 2) },
        LastThread { home: String::new(), ..row(3, 3) },
    ]);
    assert!(decode(&bad).is_empty(), "a field that could not go on a command line drops the row");
    assert!(decode("1\t2\n").is_empty());
    let odd = decode(&encode(&[LastThread { model: Some("gpt 5; x".into()), cwd: Some("/it's".into()), ..row(4, 4) }]));
    assert_eq!((odd[0].model.as_deref(), odd[0].cwd.as_deref()), (None, None), "an unsafe model or directory is left off");
}

/// One thread is one pane's: of two rows naming it, the later stays.
#[test]
fn a_thread_named_by_two_rows_is_kept_once() {
    let first = row(1, 10);
    let second = LastThread { thread: first.thread.clone(), ..row(2, 20) };
    let rows = decode(&encode(&[first, second, row(3, 30)]));
    assert_eq!(rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![2, 3]);
}

#[test]
fn upsert_keeps_a_known_model_and_moves_a_thread_between_panes() {
    let mut rows = vec![row(1, 10)];
    assert!(!upsert(&mut rows, LastThread { model: None, ..row(1, 10) }), "nothing new");
    assert_eq!(rows[0].model.as_deref(), Some("gpt-5.5"));
    assert!(upsert(&mut rows, LastThread { thread: thread(1), ..row(2, 20) }), "the thread moved to pane 2");
    assert_eq!(rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![2]);
    for sid in 10..10 + ROWS_CAP as u64 {
        upsert(&mut rows, row(sid, sid as i32));
    }
    assert_eq!(rows.len(), ROWS_CAP);
    assert!(rows.iter().all(|r| r.sid != 2), "the oldest goes");
}

#[test]
fn the_thread_a_command_line_resumes() {
    let argv = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
    assert_eq!(argv_thread(&argv(&format!("codex resume {THREAD}"))), Some(THREAD));
    assert_eq!(argv_thread(&argv(&format!("codex resume {THREAD} -m gpt-5.5"))), Some(THREAD));
    assert_eq!(argv_thread(&argv("codex resume --last")), None);
    assert_eq!(argv_thread(&argv("codex")), None);
    assert_eq!(argv_thread(&argv("codex resume not-a-thread")), None);
}
