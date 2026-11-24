use super::*;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;

/// A scratch HOME with `~/.codex-shared` and two profiles whose
/// sessions are the shared ones, each with a lock directory of its own.
pub(super) fn layout() -> PathBuf {
    let h = std::env::temp_dir().join(format!("pb-codex-locks-{}", marspot_peer::rand::hex(6)));
    let h = {
        fs::create_dir_all(&h).unwrap();
        h.canonicalize().unwrap()
    };
    fs::create_dir_all(h.join(".codex-shared/sessions")).unwrap();
    for n in [1, 2] {
        let p = h.join(format!(".codex-profile-{n}"));
        fs::create_dir_all(p.join(DIR)).unwrap();
        symlink(h.join(".codex-shared/sessions"), p.join("sessions")).unwrap();
        fs::write(p.join(DIR).join(".coordination.lock"), "").unwrap();
    }
    symlink(h.join(".codex-profile-1"), h.join(".codex")).unwrap();
    h
}

/// A listening control socket for `home`, a link into /tmp as codex
/// makes it.
fn listen(home: &Path) -> (std::os::unix::net::UnixListener, PathBuf) {
    let real = PathBuf::from(format!("/tmp/pbcl-{}.sock", marspot_peer::rand::hex(6)));
    let l = std::os::unix::net::UnixListener::bind(&real).unwrap();
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    symlink(&real, home.join(SOCKET)).unwrap();
    (l, real)
}

pub(super) fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

fn link_of(p: &Path) -> Option<PathBuf> {
    fs::read_link(p).ok()
}

#[test]
fn two_profiles_share_one_lock_directory_and_keep_every_entry() {
    let h = layout();
    let (p1, p2, shared) = (h.join(".codex-profile-1"), h.join(".codex-profile-2"), h.join(".codex-shared"));
    fs::write(p1.join(DIR).join("t1.lock"), "").unwrap();
    fs::write(p2.join(DIR).join("t2.lock"), "").unwrap();
    // the same thread in both: the shared one stays, by inode
    fs::write(p2.join(DIR).join("t1.lock"), "").unwrap();
    assert_eq!(homes(&h), vec![p1.clone(), p2.clone()], "~/.codex is profile 1, once");
    let ino = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
    // profile 1 goes first, so its file is the one that stays
    let first = ino(&p1.join(DIR).join("t1.lock"));
    assert_ne!(first, ino(&p2.join(DIR).join("t1.lock")));
    let notes = reconcile_for(&h);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(share(&p1, &shared), Outcome::Already);
    assert_eq!(link_of(&p1.join(DIR)), Some(shared.join(DIR)));
    assert_eq!(link_of(&p2.join(DIR)), Some(shared.join(DIR)));
    assert_eq!(names(&shared.join(DIR)), vec![".coordination.lock", "t1.lock", "t2.lock"]);
    assert_eq!(first, ino(&shared.join(DIR).join("t1.lock")), "the shared directory keeps profile 1's inode");
    assert!(!p2.join(LEFT).exists() && !p1.join(LEFT).exists());
    assert!(reconcile_for(&h).is_empty(), "a second pass has nothing to say");
    fs::remove_dir_all(&h).unwrap();
}

fn reconcile_for(h: &Path) -> Vec<String> {
    let shared = h.join(".codex-shared");
    homes(h).iter().filter_map(|x| share(x, &shared).note()).collect()
}

#[test]
fn an_entry_that_is_no_empty_lock_file_is_kept_aside_and_nothing_is_lost() {
    let h = layout();
    let (p1, p2, shared) = (h.join(".codex-profile-1"), h.join(".codex-profile-2"), h.join(".codex-shared"));
    assert!(matches!(share(&p1, &shared), Outcome::Shared { moved: 1, .. }));
    fs::write(p2.join(DIR).join(".coordination.lock"), "something").unwrap();
    assert_eq!(share(&p2, &shared), Outcome::Shared { moved: 0, duplicates: 0, left: 1 });
    assert_eq!(fs::read_to_string(p2.join(LEFT).join(".coordination.lock")).unwrap(), "something");
    assert_eq!(link_of(&p2.join(DIR)), Some(shared.join(DIR)));
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_home_without_the_shared_sessions_or_with_a_foreign_link_is_not_touched() {
    let h = layout();
    let shared = h.join(".codex-shared");
    let own = h.join(".codex-profile-3");
    fs::create_dir_all(own.join("sessions")).unwrap();
    fs::create_dir_all(own.join(DIR)).unwrap();
    assert_eq!(share(&own, &shared), Outcome::NotShared);
    assert!(fs::symlink_metadata(own.join(DIR)).unwrap().is_dir());
    let p2 = h.join(".codex-profile-2");
    fs::remove_dir_all(p2.join(DIR)).unwrap();
    symlink("/elsewhere", p2.join(DIR)).unwrap();
    assert!(matches!(share(&p2, &shared), Outcome::LeftAlone(_)));
    assert_eq!(link_of(&p2.join(DIR)), Some(PathBuf::from("/elsewhere")));
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_home_whose_app_server_answers_is_skipped_until_it_does_not() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let (listener, sock) = listen(&p1);
    assert!(matches!(share(&p1, &shared), Outcome::Busy(w) if w.contains("answers")));
    assert!(fs::symlink_metadata(p1.join(DIR)).unwrap().is_dir(), "untouched");
    // the socket file of a daemon that is gone refuses the connection
    drop(listener);
    assert!(sock.exists());
    assert_eq!(busy(&p1), None);
    assert!(matches!(share(&p1, &shared), Outcome::Shared { .. }));
    fs::remove_file(&sock).unwrap();
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_daemon_pid_counts_only_for_the_process_that_wrote_it() {
    let h = layout();
    let p1 = h.join(".codex-profile-1");
    fs::create_dir_all(p1.join("app-server-daemon")).unwrap();
    let me = std::process::id() as i32;
    let start = marspot::pidtree::proc_row(me).unwrap().start_unix;
    let write = |pid: i32, start: i64| {
        let text = format!(r#"{{"pid":{pid},"processIdentity":{{"startSeconds":{start}}}}}"#);
        fs::write(p1.join(PID_FILE), text).unwrap();
    };
    write(me, start as i64);
    assert_eq!(busy(&p1), Some(format!("its daemon (pid {me}) is alive")));
    write(me, start as i64 + 1);
    assert_eq!(busy(&p1), Some(format!("its daemon (pid {me}) is alive")), "a second off");
    write(me, start as i64 - 1);
    assert!(busy(&p1).is_some(), "a second off the other way");
    write(me, start as i64 + 2);
    assert_eq!(busy(&p1), None, "two seconds off is another process");
    write(me, start as i64 - 1000);
    assert_eq!(busy(&p1), None, "a pid reused by another process");
    let mut dead = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let gone = dead.id() as i32;
    dead.wait().unwrap();
    write(gone, start as i64);
    assert_eq!(busy(&p1), None, "a pid that is gone");
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_held_lock_keeps_the_home_as_it_is() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let lock = p1.join(DIR).join("t.lock");
    fs::write(&lock, "").unwrap();
    // another open file description, as another process would have
    let f = fs::File::open(&lock).unwrap();
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert!(held(&lock));
    assert!(matches!(share(&p1, &shared), Outcome::Busy(w) if w.contains("t.lock")));
    assert!(lock.exists(), "a held lock file is never moved");
    drop(f);
    assert!(!held(&lock));
    assert!(matches!(share(&p1, &shared), Outcome::Shared { moved: 2, .. }));
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn restore_gives_a_home_its_own_directory_and_no_pass_shares_it_again() {
    let h = layout();
    let (p1, p2, shared) = (h.join(".codex-profile-1"), h.join(".codex-profile-2"), h.join(".codex-shared"));
    fs::write(p1.join(DIR).join("t1.lock"), "").unwrap();
    reconcile_for(&h);
    assert_eq!(restore(&p1, &shared), Ok("restored: a directory of its own".to_string()));
    assert!(fs::symlink_metadata(p1.join(DIR)).unwrap().is_dir());
    assert!(names(&p1.join(DIR)).is_empty());
    assert_eq!(names(&shared.join(DIR)), vec![".coordination.lock", "t1.lock"], "the other home keeps the shared ones");
    assert_eq!(share(&p1, &shared), Outcome::KeepsOwn);
    assert_eq!(reconcile_for(&h), Vec::<String>::new());
    assert_eq!(restore(&p1, &shared), Ok("already has its own directory".to_string()));
    // sharing by hand takes the mark away
    assert_eq!(cli_share(&p1, &h), 0);
    assert_eq!(link_of(&p1.join(DIR)), Some(shared.join(DIR)));
    // a busy home is not restored
    let (_l, sock) = listen(&p2);
    assert!(restore(&p2, &shared).unwrap_err().contains("answers"));
    assert_eq!(link_of(&p2.join(DIR)), Some(shared.join(DIR)));
    fs::remove_file(&sock).unwrap();
    fs::remove_dir_all(&h).unwrap();
}

/// `share` as the command line does it, with `h` for `~/.codex-shared`'s
/// home, without the process-wide HOME.
fn cli_share(home: &Path, h: &Path) -> i32 {
    let _ = fs::remove_file(home.join(KEEP_OWN));
    match share(home, &h.join(".codex-shared")) {
        Outcome::Shared { .. } | Outcome::Already => 0,
        _ => 1,
    }
}

#[test]
fn a_lock_taken_after_the_busy_look_is_neither_moved_nor_removed() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let target = shared.join(DIR);
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("t.lock"), "").unwrap();
    let lock = p1.join(DIR).join("t.lock");
    fs::write(&lock, "").unwrap();
    let ino = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
    let (mine, theirs) = (ino(&lock), ino(&target.join("t.lock")));
    // a codex that started after `busy` looked holds the home's own file
    let f = fs::File::open(&lock).unwrap();
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert!(matches!(move_into(&p1, &target, true), Outcome::Busy(w) if w.contains("t.lock")));
    assert_eq!(ino(&lock), mine, "the held file is still where its holder has it");
    assert_eq!(ino(&target.join("t.lock")), theirs);
    assert!(fs::symlink_metadata(p1.join(DIR)).unwrap().is_dir(), "nothing changed");
    assert_eq!(names(&p1.join(DIR)), vec![".coordination.lock", "t.lock"]);
    assert_eq!(names(&target), vec!["t.lock"]);
    drop(f);
    // a codex making a lock file holds the coordination lock meanwhile
    let c = fs::File::open(p1.join(DIR).join(COORD)).unwrap();
    assert_eq!(unsafe { libc::flock(c.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert!(matches!(move_into(&p1, &target, true), Outcome::Busy(w) if w.contains(COORD)));
    assert_eq!(names(&p1.join(DIR)), vec![".coordination.lock", "t.lock"]);
    drop(c);
    assert_eq!(move_into(&p1, &target, true), Outcome::Shared { moved: 1, duplicates: 1, left: 0 });
    assert_eq!(ino(&target.join("t.lock")), theirs);
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_live_daemon_is_known_by_its_pid_file_without_connecting_to_it() {
    let h = layout();
    let p1 = h.join(".codex-profile-1");
    let (listener, sock) = listen(&p1);
    listener.set_nonblocking(true).unwrap();
    fs::create_dir_all(p1.join("app-server-daemon")).unwrap();
    let me = std::process::id() as i32;
    let start = marspot::pidtree::proc_row(me).unwrap().start_unix;
    let text = format!(r#"{{"pid":{me},"processIdentity":{{"startSeconds":{start}}}}}"#);
    fs::write(p1.join(PID_FILE), text).unwrap();
    assert!(busy(&p1).is_some_and(|w| w.contains("daemon")));
    let e = listener.accept().map(|_| ()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "the app-server was sent no connection");
    // without a valid pid file the socket is how it is known
    fs::write(p1.join(PID_FILE), "not json").unwrap();
    assert!(busy(&p1).is_some_and(|w| w.contains("answers")));
    assert!(listener.accept().is_ok());
    fs::remove_file(&sock).unwrap();
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn restore_and_share_go_round_twice_and_every_leftover_is_kept() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    assert!(matches!(share(&p1, &shared), Outcome::Shared { .. }));
    for round in 1..=2 {
        assert!(restore(&p1, &shared).is_ok());
        fs::write(p1.join(DIR).join(COORD), format!("round {round}")).unwrap();
        assert_eq!(cli_share(&p1, &h), 0, "round {round}");
        assert_eq!(link_of(&p1.join(DIR)), Some(shared.join(DIR)));
    }
    assert_eq!(fs::read_to_string(p1.join(LEFT).join(COORD)).unwrap(), "round 1");
    assert_eq!(fs::read_to_string(p1.join(format!("{LEFT}.2")).join(COORD)).unwrap(), "round 2");
    let said = restore(&p1, &shared).unwrap();
    assert!(said.contains(LEFT) && said.contains(&format!("{LEFT}.2")), "{said}");
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_link_that_reaches_the_shared_directory_another_way_is_already_shared() {
    let h = layout();
    let (p2, shared) = (h.join(".codex-profile-2"), h.join(".codex-shared"));
    fs::create_dir_all(shared.join(DIR)).unwrap();
    fs::remove_dir_all(p2.join(DIR)).unwrap();
    symlink(format!("../.codex-shared/{DIR}"), p2.join(DIR)).unwrap();
    assert_eq!(share(&p2, &shared), Outcome::Already);
    assert_eq!(restore(&p2, &shared), Ok("restored: a directory of its own".to_string()));
    fs::remove_dir_all(&h).unwrap();
}
