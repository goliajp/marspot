use super::tests::{layout, names};
use super::*;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::symlink;
use std::sync::mpsc;
use std::thread::JoinHandle;

fn ino(p: &Path) -> u64 {
    fs::metadata(p).unwrap().ino()
}

fn flock(f: &fs::File, how: i32) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), how) == 0 }
}

/// What a codex of `home` does for thread `T`: open the coordination
/// lock by name, wait for it, then open `T.lock` by name and take it.
/// Returns once the coordination file is open; the thread ends holding
/// `T.lock` (and its inode) or with nothing.
fn opener(home: &Path) -> JoinHandle<Option<(fs::File, u64)>> {
    let dir = home.join(DIR);
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        let open = |p: PathBuf| fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(p);
        let c = open(dir.join(COORD));
        tx.send(()).unwrap();
        let c = c.ok()?;
        assert!(flock(&c, libc::LOCK_EX));
        let t = open(dir.join("T.lock")).ok()?;
        if !flock(&t, libc::LOCK_EX | libc::LOCK_NB) {
            return None;
        }
        let i = t.metadata().unwrap().ino();
        Some((t, i))
    });
    rx.recv().unwrap();
    h
}

#[test]
fn the_link_is_in_place_before_any_entry_moves_and_no_thread_lock_gets_two_holders() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let (old, target) = (p1.join(DIR), shared.join(DIR));
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("T.lock"), "").unwrap();
    fs::write(target.join("x.lock"), "theirs").unwrap();
    fs::write(old.join("T.lock"), "").unwrap();
    fs::write(old.join("x.lock"), "mine").unwrap();
    fs::write(old.join("a.lock"), "").unwrap();
    let (shared_t, mine_a) = (ino(&target.join("T.lock")), ino(&old.join("a.lock")));
    let mut steps: Vec<String> = Vec::new();
    let mut racers = Vec::new();
    let out = move_into_seen(&p1, &target, true, &mut |s| {
        match &s {
            // a codex that starts now waits on the old coordination lock
            Step::Locked => racers.push(opener(&p1)),
            Step::Linked | Step::Placed(_) => {
                assert_eq!(fs::read_link(&old).ok(), Some(target.clone()), "linked before anything moves");
                racers.push(opener(&p1));
            }
        }
        if let Step::Placed(n) = &s {
            let n = n.to_str().unwrap();
            if n == "a.lock" {
                let f = fs::File::open(target.join("a.lock")).unwrap();
                assert!(flock(&f, libc::LOCK_EX | libc::LOCK_NB), "a moved file's lock is let go at once");
            }
            steps.push(n.to_string());
        } else {
            steps.push(format!("{s:?}"));
        }
    });
    assert_eq!(out, Outcome::Shared { moved: 1, duplicates: 2, left: 1 }, "{steps:?}");
    assert_eq!(steps[..2], ["Locked".to_string(), "Linked".to_string()]);
    assert_eq!(steps.last().map(String::as_str), Some(COORD), "the coordination lock goes last");
    // every racer's file stays open until all have tried
    let res: Vec<Option<(fs::File, u64)>> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let held: Vec<u64> = res.iter().flatten().map(|(_, i)| *i).collect();
    assert_eq!(held, vec![shared_t], "one holder of T, on the shared file");
    assert_eq!(ino(&target.join("T.lock")), shared_t);
    assert_eq!(ino(&target.join("a.lock")), mine_a);
    assert_eq!(fs::read_to_string(target.join("x.lock")).unwrap(), "theirs");
    assert_eq!(names(&p1.join(LEFT)), vec!["x.lock"]);
    assert_eq!(fs::read_to_string(p1.join(LEFT).join("x.lock")).unwrap(), "mine");
    let stray: Vec<String> = names(&p1).into_iter().filter(|n| n.contains(".moving-")).collect();
    assert!(stray.is_empty(), "{stray:?}");
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_link_to_a_shared_directory_that_is_gone_is_still_ours() {
    let h = layout();
    let (p1, p2, shared) = (h.join(".codex-profile-1"), h.join(".codex-profile-2"), h.join(".codex-shared"));
    assert!(matches!(share(&p1, &shared), Outcome::Shared { .. }));
    fs::remove_dir_all(p2.join(DIR)).unwrap();
    symlink(format!("../.codex-shared/{DIR}"), p2.join(DIR)).unwrap();
    fs::remove_dir_all(shared.join(DIR)).unwrap();
    assert_eq!(share(&p1, &shared), Outcome::Already);
    assert!(shared.join(DIR).is_dir(), "the shared directory is made again");
    fs::remove_dir_all(shared.join(DIR)).unwrap();
    assert_eq!(share(&p2, &shared), Outcome::Already, "a relative link too");
    fs::remove_dir_all(shared.join(DIR)).unwrap();
    assert_eq!(restore(&p1, &shared), Ok("restored: a directory of its own".to_string()));
    assert_eq!(restore(&p2, &shared), Ok("restored: a directory of its own".to_string()));
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn every_leftover_directory_is_found_across_a_gap_and_the_next_goes_above_them() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    fs::create_dir_all(p1.join(LEFT)).unwrap();
    fs::create_dir_all(p1.join(format!("{LEFT}.3"))).unwrap();
    fs::create_dir_all(p1.join(format!("{LEFT}.x"))).unwrap();
    fs::create_dir_all(shared.join(DIR)).unwrap();
    fs::write(shared.join(DIR).join(COORD), "theirs").unwrap();
    fs::write(p1.join(DIR).join(COORD), "mine").unwrap();
    assert_eq!(share(&p1, &shared), Outcome::Shared { moved: 0, duplicates: 0, left: 1 });
    assert_eq!(fs::read_to_string(p1.join(format!("{LEFT}.4")).join(COORD)).unwrap(), "mine");
    let said = restore(&p1, &shared).unwrap();
    for n in [LEFT.to_string(), format!("{LEFT}.3"), format!("{LEFT}.4")] {
        assert!(said.contains(&format!("{}", p1.join(&n).display())), "{n}: {said}");
    }
    assert!(!said.contains(&format!("{LEFT}.x")), "{said}");
    fs::remove_dir_all(&h).unwrap();
}
