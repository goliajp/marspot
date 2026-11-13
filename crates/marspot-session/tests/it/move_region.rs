//! A pane's L3 asked to publish into another grid region, end to end.
//!
//! The real `marspot-session` binary and a real shell.  The core asks
//! for this when the region the session publishes into cannot be
//! shown; the session has to arrive in the new region with the same
//! shell and the same screen, say so, and stay there across an update
//! of its own image.

use std::os::fd::AsRawFd;

use marspot_term::grid_shm::{self, ClusterTableReader, GridShmReader, Identity, SessionRegions};
use marspot_term::session_registry::read_session_entry;
use marspot_term::shell_proto::MsgType;

use crate::self_execv::{
    Link, Sandbox, id, echo_pid, read_echoed_pid, shell_pid, stage, start, wait_for,
};
use crate::support::{COLS, ROWS, Session};

/// A region made for the session to move to, as the core makes one,
/// and a reader on it.
fn region_to_move_to() -> (SessionRegions, GridShmReader) {
    let regions = SessionRegions::for_session(id());
    let fd = regions.create(COLS, ROWS).expect("a new grid region");
    let reader = GridShmReader::from_fd(fd.as_raw_fd()).expect("a reader on it");
    (regions, reader)
}

fn ask_to_move(s: &Session, regions: &SessionRegions) {
    s.send_frame(MsgType::SessionRepublish, regions.grid_name().to_bytes().to_vec());
}

/// Read what the session says it publishes into until it names the
/// region with `serial`.
fn await_arrival(s: &Session, serial: u64) {
    wait_for(10, "the session to say it publishes into the new region", || {
        let f = s.await_frame(MsgType::SessionPublishes, 10);
        let said = u64::from_le_bytes(*f.payload.first_chunk::<8>().expect("a serial"));
        (said == serial).then_some(())
    });
}

#[test]
fn a_session_asked_to_move_publishes_into_the_new_region_with_the_same_shell() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let shell_before = shell_pid(&s, "before");
    let screen_before = s.screen(COLS, ROWS);

    let (regions, reader) = region_to_move_to();
    let serial = reader.serial().expect("the region carries a serial");
    ask_to_move(&s, &regions);
    await_arrival(&s, serial);

    // from here on the test reads the new region only
    s.reader = reader;
    let arrived = wait_for(10, "the screen in the new region", || s.try_screen(COLS, ROWS));
    assert_eq!(arrived, screen_before, "the screen arrived as it was");
    assert_eq!(shell_pid(&s, "after"), shell_before, "the same shell is on the PTY");
    assert_eq!(s.child.try_wait().unwrap(), None, "the session ended");

    let entry = read_session_entry(id()).expect("the session's entry");
    let named = SessionRegions::of_entry(&entry).expect("the entry names a region");
    let (opens, clusters, title) = (
        GridShmReader::open(&named).map(|r| r.serial()),
        ClusterTableReader::open(&named).is_ok(),
        grid_shm::title::read(&named),
    );
    regions.delete();
    assert_eq!(opens.ok(), Some(Some(serial)), "the entry names the region the session moved to");
    assert!(clusters, "no cluster region beside the new grid region");
    assert!(matches!(title, Ok(Some(_))), "no title region beside the new grid region: {title:?}");
    s.kill();
}

#[test]
fn a_session_does_not_move_into_a_region_that_is_not_its_own() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let shell_before = shell_pid(&s, "before");

    let ours = SessionRegions::for_session(id());
    let stranger = Identity { session: id() + 1, ..ours.owner() };
    let fd = grid_shm::create_region_named(COLS, ROWS, ours.grid_name(), stranger).unwrap();
    let theirs = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
    ask_to_move(&s, &ours);

    // the next thing typed is answered where the session was
    assert_eq!(shell_pid(&s, "after"), shell_before);
    let (seq, entry) = (theirs.seq(), read_session_entry(id()).expect("the session's entry"));
    ours.delete();
    assert_eq!(seq, 0, "the session published into another session's region");
    assert_ne!(entry.shm_name.as_bytes(), ours.grid_name().to_bytes(), "the entry names the region it refused");
    s.kill();
}

/// The image a session updates itself to inherits the region it moved
/// to, not the one it was started with.
#[test]
fn a_session_that_moved_stays_in_the_new_region_across_its_own_update() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let pid = s.pid();
    let shell_before = shell_pid(&s, "before");
    let (regions, reader) = region_to_move_to();
    let serial = reader.serial().expect("the region carries a serial");
    ask_to_move(&s, &regions);
    await_arrival(&s, serial);
    s.reader = reader;

    let staged = stage(sandbox.path(), true).canonicalize().unwrap();
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(30, "the pid to run the staged image", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited instead of handing over");
        let running =
            marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok());
        (running.as_deref() == Some(staged.as_path())).then_some(())
    });
    let mut link = Link::open();
    let shell_after = link.send_until(
        &s,
        "after= in the region the session moved to",
        |mut stream| {
            let _ = echo_pid("after").write_to(&mut stream);
        },
        || read_echoed_pid(&s, "after"),
    );
    let named = read_session_entry(id()).ok().and_then(|e| SessionRegions::of_entry(&e));
    let opens = named.as_ref().map(|n| GridShmReader::open(n).map(|r| r.serial()).ok());
    regions.delete();
    assert_eq!(shell_after, shell_before, "the same shell is on the PTY");
    assert_eq!(opens, Some(Some(Some(serial))), "the new image's entry names another region");
    s.kill();
}
