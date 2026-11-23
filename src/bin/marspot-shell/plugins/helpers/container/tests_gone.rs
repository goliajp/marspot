//! When a container is over: its removal is bounded, and its directory
//! goes after it and not before.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::super::table::Event;
use super::super::tests::{shared_at, temp};
use super::tests::boxed;
use super::tests_away::fake;
use super::*;

fn script(dir: &Path, docker: &Path, cwd: &Path) -> String {
    let spec = script::Spec { docker, dir, sid: 4, agent: AgentKind::Kimi, binds: &[], model: None, mode: "auto", cwd };
    script::run(&spec, false).unwrap()
}

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let t = Instant::now();
    while !done() {
        assert!(t.elapsed() < Duration::from_secs(20), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn executable(at: &Path, text: &str) {
    std::fs::write(at, text).unwrap();
    std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// docker's own runtime ignores an alarm.
#[test]
fn container_removal_ends_a_docker_that_ignores_an_alarm() {
    let dir = std::fs::canonicalize(temp("container-alarm")).unwrap();
    let docker = dir.join("docker");
    executable(&docker, &format!("#!/bin/sh\ntrap '' ALRM\n[ \"$1\" = rm ] || exit 0\necho $$ > '{}/pid'\nexec sleep 60\n", dir.display()));
    std::fs::write(dir.join("image"), "i").unwrap();
    let run = script(&dir, &docker, &dir);
    assert_eq!(run.matches("limit 20 ").count(), 1);
    std::fs::write(dir.join("run.sh"), run.replace("limit 20 ", "limit 2 ")).unwrap();
    let t = Instant::now();
    Command::new("/bin/sh").arg(dir.join("run.sh")).status().unwrap();
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    let pid: i32 = std::fs::read_to_string(dir.join("pid")).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only asks
    wait("the docker command ends", || unsafe { libc::kill(pid, 0) } != 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_script_stops_when_its_directory_is_another() {
    let dir = std::fs::canonicalize(temp("container-moved")).unwrap();
    let (docker, cwd) = (dir.join("docker"), dir.join("work"));
    executable(&docker, &format!("#!/bin/sh\necho \"$1\" >> '{}/log'\n", dir.display()));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(dir.join("image"), "i").unwrap();
    // no terminal here to read from
    let text = script(&dir, &docker, &cwd).replace("</dev/tty", "</dev/null");
    std::fs::write(dir.join("run.sh"), text).unwrap();
    let run = || Command::new("/bin/sh").arg(dir.join("run.sh")).output().unwrap().status.success();
    let log = || std::fs::read_to_string(dir.join("log")).unwrap_or_default();
    run();
    assert_eq!(log(), "run\nrm\n");
    std::fs::rename(&cwd, dir.join("was")).unwrap();
    assert!(!run(), "gone");
    symlink(dir.join("was"), &cwd).unwrap();
    assert!(!run(), "a link to elsewhere");
    assert_eq!(log(), "run\nrm\n", "docker was not run");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_directory_goes_after_the_container() {
    let dir = temp("container-slow-rm");
    let d = fake(&dir);
    std::fs::write(dir.join("rm-s"), "1").unwrap();
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("PATH", format!("{}:/usr/bin:/bin", d.parent().unwrap().display()));
    let root = dir.join("helpers");
    std::fs::create_dir_all(&root).unwrap();
    let (s, _rx) = shared_at(&root);
    s.record(41, Event::Started(boxed(AgentKind::Claude)));
    pending::add(&s.root(), 41, &[]).unwrap();
    std::fs::create_dir_all(s.dir_of(41)).unwrap();
    std::fs::write(s.dir_of(41).join("env"), "x").unwrap();
    s.record(41, Event::Stopped);
    let asked = || std::fs::read_to_string(dir.join("log")).is_ok_and(|l| l.contains("rm -f"));
    wait("docker is asked", asked);
    s.sweep_dirs_where(|_| true);
    assert!(s.dir_of(41).is_dir(), "the container may still write there");
    assert!(!s.dir_of(41).join("env").exists());
    wait("then it goes", || !s.dir_of(41).exists());
    assert!(pending::all(&s.root()).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_directory_stays_until_a_start_could_remove_the_container() {
    let dir = temp("container-left");
    let d = fake(&dir);
    let root = dir.join("helpers");
    std::fs::create_dir_all(root.join("41")).unwrap();
    std::fs::create_dir_all(root.join("42")).unwrap();
    for sid in [41, 42] {
        pending::add(&root, sid, &[]).unwrap();
    }
    std::fs::write(dir.join("rm-says"), "removal in progress").unwrap();
    cleanup::sweep_now(&root, &d, |sid| sid == 42);
    assert!(root.join("41").is_dir());
    let (s, _rx) = shared_at(&root);
    assert!(!s.dir_is_over(41, |_| true), "left for the next start");
    std::fs::write(dir.join("rm-says"), "No such container: x").unwrap();
    cleanup::sweep_now(&root, &d, |sid| sid == 42);
    assert!(!root.join("41").exists() && root.join("42").is_dir());
    assert_eq!(pending::all(&root), [42].into());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_leaves_only_regular_files_among_a_login() {
    let dir = temp("container-aside");
    let (root, creds) = (dir.join("helpers"), dir.join("account/credentials"));
    let secret: PathBuf = dir.join("secret");
    std::fs::create_dir_all(creds.join("sub/deep")).unwrap();
    std::fs::create_dir_all(creds.join("kimi-code.lock")).unwrap();
    std::fs::write(&secret, "host").unwrap();
    std::fs::write(creds.join("kimi-code.json"), "{}").unwrap();
    symlink(&secret, creds.join("a.json")).unwrap();
    assert!(Command::new("/usr/bin/mkfifo").arg(creds.join("b.json")).status().unwrap().success());
    pending::add(&root, 41, &[&creds]).unwrap();
    pending::gone(&root, 41);
    let names = |d: &Path| {
        let mut n: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        n.sort();
        n
    };
    assert_eq!(names(&creds), ["kimi-code.json", "kimi-code.lock"]);
    assert_eq!(names(&dir.join("account/credentials-set-aside-41")), ["a.json", "b.json", "sub"]);
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "host");
    std::fs::remove_dir_all(&dir).unwrap();
}
