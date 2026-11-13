// A core taking over a live session whose grid region it cannot show.
// The session is a real one, with a real shell.  Included into the
// boot-assembly tests, whose sandbox it uses.

use std::time::{Duration, Instant};

use super::*;
use marspot::grid_shm::{GridShmReader, GridShmWriter, Identity, SessionRegions};

const MARK: &str = "KEEP2ME";

fn screen(pane: &Pane) -> String {
    pane.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n")
}

/// Hand what the pane's session says about its region to the pane, as
/// the core's loop does.
fn pump(pane: &mut Pane, rx: &mpsc::Receiver<CoreEvent>) {
    while let Ok(ev) = rx.try_recv() {
        if let CoreEvent::L3Publishes(_, payload) = ev {
            pane.session_mut().l3_session_publishes(&payload);
        }
    }
    pane.pump();
}

fn wait_for_mark(pane: &mut Pane, rx: &mpsc::Receiver<CoreEvent>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        pump(pane, rx);
        if screen(pane).contains(MARK) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}:\n{}", screen(pane));
}

/// A live session with `MARK` on its screen, left running with no
/// core attached.  The mark is the output of the command, not the
/// command as typed.
fn session_showing_the_mark(tx: &mpsc::Sender<CoreEvent>, rx: &mpsc::Receiver<CoreEvent>) -> SessionEntry {
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", tx)
        .expect("real L3 spawn (is marspot-session built?)");
    let deadline = Instant::now() + Duration::from_secs(30);
    'typed: while Instant::now() < deadline {
        pane.session_mut().forward_inject_input(b"clear; echo KEEP$((1+1))ME\r");
        for _ in 0..40 {
            pump(&mut pane, rx);
            if screen(&pane).contains(MARK) {
                break 'typed;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(screen(&pane).contains(MARK), "the shell never printed the mark:\n{}", screen(&pane));
    let entry = reg::read_session_entry(sid).expect("the session's entry");
    // the sandbox ends the session; dropping the pane would
    std::mem::forget(pane);
    entry
}

fn take_over(entry: &SessionEntry, tx: &mpsc::Sender<CoreEvent>) -> Pane {
    let s = saved(&[(entry.id, "")]);
    let (mut panes, reattached) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, tx, true, &Default::default());
    assert_eq!(reattached, vec![entry.id], "the live session was not taken over");
    assert_eq!(panes.len(), 1);
    panes.remove(0)
}

fn assert_same_processes(before: &SessionEntry) {
    let now = reg::read_session_entry(before.id).expect("the session's entry");
    assert_eq!(now.pid, before.pid, "the session was started again");
    assert_eq!(now.shell_child_pid, before.shell_child_pid, "the pane has another shell");
    for pid in [before.pid, before.shell_child_pid] {
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "pid {pid} is gone");
    }
}

/// Another program removed the name of the session's grid region.
/// The core that starts next cannot open it: the session moves to a
/// region the core makes, with the same shell and the same screen.
#[test]
fn a_session_whose_grid_region_lost_its_name_is_given_another() {
    let _sb = Sandbox::new();
    let (tx, rx) = mpsc::channel();
    let before = session_showing_the_mark(&tx, &rx);
    let regions = SessionRegions::of_entry(&before).unwrap();
    marspot::grid_shm::delete_region(regions.grid_name());
    assert!(GridShmReader::open(&regions).is_err(), "the name is still there");

    let mut pane = take_over(&before, &tx);
    wait_for_mark(&mut pane, &rx, "the screen did not come back after the session moved");
    assert_same_processes(&before);
    let now = reg::read_session_entry(before.id).unwrap();
    let moved = SessionRegions::of_entry(&now).unwrap();
    let opens = GridShmReader::open(&moved).is_ok();
    moved.delete();
    assert!(opens, "the entry does not name a region that opens");
    std::mem::forget(pane);
}

/// Another session's region sits under the name this session's entry
/// gives.  The core shows nothing of it, and the session moves.
#[test]
fn a_region_of_another_session_under_the_name_is_not_shown() {
    let _sb = Sandbox::new();
    let (tx, rx) = mpsc::channel();
    let before = session_showing_the_mark(&tx, &rx);
    let regions = SessionRegions::of_entry(&before).unwrap();
    let stranger = Identity { state: regions.owner().state ^ 1, ..regions.owner() };
    let fd = marspot::grid_shm::create_region_named(60, 16, regions.grid_name(), stranger).unwrap();
    let mut theirs = GridShmWriter::from_fd(fd).unwrap();
    let mut term = marspot::terminal::Terminal::new(60, 16);
    term.feed(b"INTRUDER");
    theirs.publish(term.grid(), 0, 0);

    let mut pane = take_over(&before, &tx);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !screen(&pane).contains(MARK) {
        pump(&mut pane, &rx);
        assert!(!screen(&pane).contains("INTRUDER"), "another session's screen was shown");
        std::thread::sleep(Duration::from_millis(20));
    }
    regions.delete();
    assert!(screen(&pane).contains(MARK), "the session's own screen never came back:\n{}", screen(&pane));
    assert_same_processes(&before);
    std::mem::forget(pane);
}

/// A session from a build that cannot move, with nothing staged for it
/// to update to, and no region to show: the one case left in which the
/// core starts the session again.
#[test]
fn a_session_that_cannot_move_and_cannot_update_is_started_again() {
    let _sb = Sandbox::new();
    let (tx, rx) = mpsc::channel();
    let before = session_showing_the_mark(&tx, &rx);
    // as a session from before identities wrote its entry
    let mut old = before.clone();
    old.stamps_regions = false;
    write_session_entry(&old).unwrap();
    marspot::grid_shm::delete_region(SessionRegions::of_entry(&before).unwrap().grid_name());

    let s = saved(&[(before.id, "")]);
    let (panes, reattached) =
        assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &Default::default());
    assert!(reattached.is_empty(), "taken over with no region to show");
    assert_eq!(pane_sids(&panes), vec![before.id], "the slot keeps its session id");
    for pane in panes {
        std::mem::forget(pane);
    }
    if let Some(r) = reg::read_session_entry(before.id).ok().and_then(|e| SessionRegions::of_entry(&e)) {
        r.delete();
    }
}
