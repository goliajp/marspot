use super::tests::{layout, names};
use super::*;
use std::fs;
use std::os::unix::fs::symlink;

fn moving_dir(home: &Path, pid: u32) -> PathBuf {
    home.join(format!(".{DIR}.moving-{pid}"))
}

#[test]
fn a_pass_that_failed_after_the_swap_is_finished_by_the_next_one() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let target = shared.join(DIR);
    fs::create_dir_all(&target).unwrap();
    fs::write(p1.join(DIR).join("t.lock"), "").unwrap();
    fs::write(p1.join(DIR).join("x.lock"), "mine").unwrap();
    // the shared directory turns into a file once the link is in, so
    // every move into it fails
    let out = move_into_seen(&p1, &target, true, &mut |s| {
        if s == Step::Linked {
            fs::remove_dir(&target).unwrap();
            fs::write(&target, "").unwrap();
        }
    });
    assert!(matches!(&out, Outcome::Failed(w) if w.contains("the rest is in")), "{out:?}");
    let old = moving_dir(&p1, std::process::id());
    assert_eq!(names(&old), vec![".coordination.lock", "t.lock", "x.lock"]);
    assert_eq!(leftovers(&p1), vec![old.clone()]);
    fs::remove_file(&target).unwrap();
    // the home is the link now; a later pass finds the old directory
    assert_eq!(share(&p1, &shared), Outcome::Shared { moved: 3, duplicates: 0, left: 0 });
    assert!(!old.exists());
    assert_eq!(names(&target), vec![".coordination.lock", "t.lock", "x.lock"]);
    assert_eq!(fs::read_to_string(target.join("x.lock")).unwrap(), "mine");
    assert_eq!(share(&p1, &shared), Outcome::Already);
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn an_old_directory_left_by_a_crash_is_listed_and_then_merged() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let target = shared.join(DIR);
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("t.lock"), "").unwrap();
    // what a crash right after the swap leaves: the link, and the old
    // directory under its moving name
    let old = moving_dir(&p1, 99_999);
    fs::rename(p1.join(DIR), &old).unwrap();
    symlink(&target, p1.join(DIR)).unwrap();
    fs::write(old.join("t.lock"), "").unwrap();
    fs::write(old.join("u.lock"), "").unwrap();
    assert_eq!(leftovers(&p1), vec![old.clone()]);
    assert_eq!(share(&p1, &shared), Outcome::Shared { moved: 2, duplicates: 1, left: 0 });
    assert!(!old.exists());
    assert_eq!(names(&target), vec![".coordination.lock", "t.lock", "u.lock"]);
    assert!(leftovers(&p1).is_empty());
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_dangling_moving_link_of_this_pid_does_not_stop_a_share() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let stale = moving_dir(&p1, std::process::id());
    symlink("/nowhere-at-all", &stale).unwrap();
    assert!(matches!(share(&p1, &shared), Outcome::Shared { moved: 1, .. }));
    assert!(fs::symlink_metadata(&stale).is_err(), "the stale link is gone");
    assert_eq!(fs::read_link(p1.join(DIR)).unwrap(), shared.join(DIR));
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_file_made_in_the_old_directory_during_the_move_is_kept_aside() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    let target = shared.join(DIR);
    fs::create_dir_all(&target).unwrap();
    let old = moving_dir(&p1, std::process::id());
    let out = move_into_seen(&p1, &target, true, &mut |s| {
        if s == Step::Placed(std::ffi::OsStr::new(COORD)) {
            fs::write(old.join("late.lock"), "").unwrap();
        }
    });
    assert_eq!(out, Outcome::Shared { moved: 1, duplicates: 0, left: 1 });
    assert!(!old.exists());
    assert_eq!(names(&p1.join(LEFT)), vec!["late.lock"]);
    fs::remove_dir_all(&h).unwrap();
}

#[test]
fn restore_is_refused_while_a_process_runs_under_the_home() {
    let h = layout();
    let (p1, shared) = (h.join(".codex-profile-1"), h.join(".codex-shared"));
    assert!(matches!(share(&p1, &shared), Outcome::Shared { .. }));
    // this test binary, not a system one: macOS hides the environment
    // of hardened system binaries from other processes
    let exe = std::env::current_exe().unwrap();
    let name = "plugins::peerbus::codex_locks::recover_tests::sleeps_under_its_codex_home";
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--exact", name, "--ignored"]).env("CODEX_HOME", &p1).stdout(std::process::Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let pid = child.id() as i32;
    let t0 = std::time::Instant::now();
    while marspot::pidtree::proc_env_value(pid, "CODEX_HOME").is_none() {
        assert!(t0.elapsed().as_secs() < 5, "the environment of {pid} is not readable");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let said = restore(&p1, &shared).unwrap_err();
    assert!(said.contains(&format!("pid {}", child.id())), "{said}");
    assert_eq!(fs::read_link(p1.join(DIR)).unwrap(), shared.join(DIR), "untouched");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(restore(&p1, &shared), Ok("restored: a directory of its own".to_string()));
    fs::remove_dir_all(&h).unwrap();
}

/// The process `restore_is_refused_while_a_process_runs_under_the_home`
/// starts; run only by it.
#[test]
#[ignore]
fn sleeps_under_its_codex_home() {
    if std::env::var_os("CODEX_HOME").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
