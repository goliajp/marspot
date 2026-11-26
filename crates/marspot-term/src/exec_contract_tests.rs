//! The contract with real processes: this test binary started again as
//! the new image, once swept and once with something extra.

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

const STAGE: &str = "MARSPOT_TEST_EXEC_CONTRACT_STAGE";
const CHILD: &str = "exec_contract::tests::exec_contract_child";
/// Not listed for any exec, on purpose.
const UNLISTED: &str = "MARSPOT_UNLISTED_BY_TEST";

/// Not a test of its own: the new image of the ones below.  Prints
/// what it found against the contract.
#[test]
fn exec_contract_child() {
    let Ok(stage) = std::env::var(STAGE) else { return };
    // SAFETY: the test runs alone in this process (--test-threads=1)
    let arrival = unsafe { arrive() }.expect("started by a marspot exec");
    match stage.as_str() {
        "report" => println!("found: {}", arrival.mismatch(&[]).unwrap_or_else(|| "nothing extra".into())),
        _ => arrival.verify(&[]),
    }
}

fn child(stage: &str, kind: ExecKind) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args([CHILD, "--exact", "--nocapture", "--test-threads=1"]).env(STAGE, stage);
    c.env(KIND_ENV, kind.mark());
    c
}

/// A descriptor without the flag, the way a leak arrives.
fn leaky() -> std::fs::File {
    let f = std::fs::File::open("/dev/null").unwrap();
    // SAFETY: fcntl on a descriptor of ours
    unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFD, 0) };
    f
}

fn swept(c: &mut Command, kind: ExecKind) {
    sweep_env(kind, c);
    let limit = crate::inherited_fds::close_limit();
    let keep: Vec<RawFd> = kind.carried(|_| None, &[]);
    let keep: &'static [RawFd] = Box::leak(keep.into_boxed_slice());
    // SAFETY: only fcntl between fork and exec
    unsafe {
        c.pre_exec(move || {
            crate::inherited_fds::close_inherited(limit, keep);
            Ok(())
        });
    }
}

fn output(mut c: Command) -> (bool, String) {
    let out = c.output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn exec_contract_a_swept_exec_hands_over_nothing_extra() {
    let _leak = leaky();
    let mut c = child("verify", ExecKind::SessionSpawn);
    c.env("MARSPOT_SESSION_ID", "7");
    swept(&mut c, ExecKind::SessionSpawn);
    let (ok, text) = output(c);
    assert!(ok, "{text}");
}

/// Through the sweep the self-execs use, on this process itself: a
/// descriptor it holds without the flag does not reach the new image.
#[test]
fn exec_contract_the_command_sweep_takes_a_descriptor_out_of_the_way() {
    let leak = leaky();
    // above the numbers any kind carries, so that no list keeps it
    // SAFETY: fcntl on a descriptor of ours; F_DUPFD leaves the flag clear
    let high = unsafe { libc::fcntl(leak.as_raw_fd(), libc::F_DUPFD, 50) };
    assert!(high >= 50 && open_without_cloexec().contains(&high), "{high}");
    let mut c = child("verify", ExecKind::SessionSpawn);
    prepare_command_exec(ExecKind::SessionSpawn, &mut c, &[]);
    assert!(!open_without_cloexec().contains(&high), "the sweep left fd {high} without the flag");
    let (ok, text) = output(c);
    assert!(ok, "fd {high} reached the new image: {text}");
}

/// The same for a variable of this process's that the kind does not
/// carry.
#[test]
fn exec_contract_the_command_sweep_takes_a_variable_out_of_the_way() {
    // SAFETY: nextest runs each test in a process of its own
    unsafe { std::env::set_var("MARSPOT_LEAKED_BY_TEST", "1") };
    let mut c = child("verify", ExecKind::SessionSpawn);
    prepare_command_exec(ExecKind::SessionSpawn, &mut c, &[]);
    let (ok, text) = output(c);
    assert!(ok, "the variable reached the new image: {text}");
}

#[test]
fn exec_contract_an_extra_descriptor_is_found() {
    let leak = leaky();
    let (_, text) = output(child("report", ExecKind::SessionSpawn));
    let fd = leak.as_raw_fd();
    let fds = text.split("fds [").nth(1).and_then(|r| r.split(']').next()).unwrap_or_else(|| panic!("{text}"));
    assert!(fds.split(", ").any(|f| f == fd.to_string()), "fd {fd} not found: {text}");
    if cfg!(debug_assertions) {
        let (ok, text) = output(child("verify", ExecKind::SessionSpawn));
        assert!(!ok, "the new image went on with fd {fd}: {text}");
    }
}

#[test]
fn exec_contract_an_extra_variable_is_found() {
    let mut c = child("report", ExecKind::SessionSpawn);
    swept(&mut c, ExecKind::SessionSpawn);
    // after the sweep, as a site that set it past the contract would
    c.env(UNLISTED, "1");
    let (_, text) = output(c);
    assert!(text.contains(&format!("variables [\"{UNLISTED}\"]")), "{text}");
    assert!(!text.contains("fds [3"), "the sweep left a descriptor: {text}");
}

#[test]
fn exec_contract_the_sweep_removes_what_the_kind_does_not_carry() {
    let mut c = Command::new("/usr/bin/env");
    c.env(crate::shell_proto::ENV_CONTROL_FD, "3").env("MARSPOT_STATE_DIR", "/tmp/x").env("MARSPOT_DEV_SANDBOX", "1");
    // a core's variables do not reach a session
    c.env_remove(crate::shell_proto::ENV_CONTROL_FD);
    sweep_env(ExecKind::SessionSpawn, &mut c);
    let (_, text) = output(c);
    assert!(text.contains("MARSPOT_STATE_DIR=/tmp/x") && text.contains("MARSPOT_DEV_SANDBOX=1"), "{text}");
    assert!(text.contains(&format!("MARSPOT_EXEC_KIND=session-spawn:{}", std::process::id())), "{text}");
    assert!(!text.contains("MARSPOT_SHELL_CONTROL_FD"), "{text}");
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "not listed")]
fn exec_contract_setting_an_unlisted_variable_panics_in_debug() {
    let mut c = Command::new("/usr/bin/true");
    c.env(UNLISTED, "1");
    sweep_env(ExecKind::CoreSpawn, &mut c);
}

#[test]
fn exec_contract_a_mark_from_a_grandparent_is_not_taken() {
    let mut c = child("report", ExecKind::SessionSpawn);
    c.env(KIND_ENV, "session-spawn:1");
    let (ok, text) = output(c);
    assert!(!ok && text.contains("started by a marspot exec"), "the child took a mark it did not get: {text}");
}

#[test]
fn exec_contract_kinds_round_trip_and_hand_over_named_descriptors() {
    for k in ExecKind::ALL {
        assert_eq!(ExecKind::parse(k.name()), Some(k));
    }
    let env = |v: &str| (v == crate::grid_shm::ENV_SHM_FD).then(|| "4".to_string());
    assert_eq!(ExecKind::SessionUpdate.carried(env, &[9, 10, -1, 12]), [9, 10, 12, 4]);
    assert_eq!(ExecKind::ShellIdentity.carried(|_| None, &[]), Vec::<RawFd>::new());
}

/// Literals that name a `MARSPOT_*` variable on purpose without its
/// being one, and build-time names rustc sets.
const NOT_RUNTIME: &[&str] = &[
    "MARSPOT_", "MARSPOT_VERSION_CORE", "MARSPOT_VERSION_SHELL", "MARSPOT_VERSION_SESSION", "MARSPOT_GIT_SHA",
    "MARSPOT_BUILD_TS", "MARSPOT_NO_SUCH_VARIABLE", UNLISTED, "MARSPOT_LEAKED_BY_TEST", KIND_ENV,
];

fn names_in(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            if !p.ends_with("target") {
                names_in(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, _) in text.match_indices("\"MARSPOT_") {
                let rest = &text[i + 1..];
                let name: String = rest.chars().take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_').collect();
                if rest[name.len()..].starts_with('"') {
                    out.push((name, p.clone()));
                }
            }
        }
    }
}

/// Every variable name in the workspace's sources is listed somewhere:
/// a name in no list would be removed at the first exec.
#[test]
fn exec_contract_every_variable_in_the_sources_is_listed() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut names = Vec::new();
    for d in ["src", "crates", "tests"] {
        names_in(&root.join(d), &mut names);
    }
    assert!(names.len() > 100, "found {} names: looking in the wrong place", names.len());
    let unlisted: Vec<_> = names
        .iter()
        .filter(|(n, _)| {
            !NOT_RUNTIME.contains(&n.as_str())
                && !AMBIENT_PREFIXES.contains(&n.as_str())
                && !ExecKind::ALL.iter().any(|k| k.vars().contains(&n.as_str()))
                && !(AMBIENT.contains(&n.as_str()) || AMBIENT_PREFIXES.iter().any(|p| n.starts_with(p)))
        })
        .collect();
    assert!(unlisted.is_empty(), "listed for no exec and not ambient: {unlisted:?}");
}
