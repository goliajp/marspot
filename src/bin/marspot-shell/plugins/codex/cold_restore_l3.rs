//! A cold restore against real session processes and the codex
//! stand-in: a pane's session process is hung up the way a shutting-down
//! machine does it, a new one comes back on the same id with the saved
//! screen and a new shell, and the pane's thread is resumed in it under
//! the home it ran under.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

use crate::plugins::l3_test_support::{spawn_l3, until, Proxy, Root};

fn now() -> Arc<Observation> {
    crate::observe::shared().fresh(crate::observe::NOW)
}

fn shell_of(sid: u64) -> Option<ProcId> {
    now().session(sid).and_then(|s| s.shell)
}

fn codex_in(sid: u64) -> Option<ProcId> {
    now().session(sid)?.find("codex").map(|p| p.row.id())
}

/// The compiled stand-in `codex` (bin/codex-standin/launcher.c) at `out`.
fn build_standin(out: &Path) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/codex-standin");
    let py = std::process::Command::new("/bin/sh").args(["-c", "command -v python3"]).output().unwrap();
    let py = String::from_utf8(py.stdout).unwrap().trim().to_string();
    assert!(!py.is_empty(), "the stand-in needs python3 on PATH");
    let status = std::process::Command::new("cc")
        .arg("-O1")
        .arg(format!("-DPYTHON=\"{py}\""))
        .arg(format!("-DSTANDIN=\"{}\"", src.join("standin.py").display()))
        .arg("-o")
        .arg(out)
        .arg(src.join("launcher.c"))
        .status()
        .unwrap();
    assert!(status.success(), "the stand-in builds");
}

/// The thread's rollout, as codex writes its first line.
fn write_rollout(home: &Path, thread: &str, cwd: &Path) {
    let dir = home.join("sessions/2026/11/28");
    fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        "{{\"timestamp\":\"2026-11-28T00:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread}\",\"session_id\":\"{thread}\",\"cwd\":\"{}\",\"originator\":\"codex-tui\",\"cli_version\":\"0.171.0\"}}}}\n",
        cwd.display()
    );
    fs::write(dir.join(format!("rollout-2026-11-28T00-00-00-{thread}.jsonl")), meta).unwrap();
}

/// End the app-server the stand-in started for `home`, by its pid file,
/// when it is ours.
fn stop_daemon(home: &Path, root: &Path) {
    let Ok(text) = fs::read_to_string(home.join("app-server-daemon/daemon.pid")) else { return };
    let Some(pid) = text.split(['{', ':', ',']).nth(2).and_then(|p| p.trim().parse::<i32>().ok()) else { return };
    if crate::plugins::pidtree::proc_cmdline(pid).is_some_and(|c| root.file_name().and_then(|n| n.to_str()).is_some_and(|n| c.contains(n))) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[test]
fn a_restarted_session_gets_its_codex_thread_back_under_the_home_it_ran_on() {
    if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
        eprintln!("SKIPPED: needs real session processes — set MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)");
        return;
    }
    let root = Root(PathBuf::from(format!("/tmp/mscx-{}", std::process::id())));
    let _ = fs::remove_dir_all(&root.0);
    let (bin, home, state, work) = (root.0.join("bin"), root.0.join("home"), root.0.join("state"), root.0.join("work"));
    let account = home.join(".codex-profile-2");
    for d in [&bin, &account, &home.join(".codex-profile-1"), &state, &work] {
        fs::create_dir_all(d).unwrap();
    }
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", &state).set("HOME", &home);
    crate::test_log::open();
    build_standin(&bin.join("codex"));
    let thread = "01a133f4-8e7b-7331-9630-9c40ff4b467f";
    write_rollout(&account, thread, &work);
    let shell = marspot_term::test_env::shell_without_rc_files(&root.0, "/bin/zsh");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // shm names are one namespace for the login: an id of this process's own
    let sid = u64::from(std::process::id()) * 100_000 + 2;
    let plugin_state = state.join("plugins/codex");

    // before the restart: codex ran the thread in the pane under P2
    let first = spawn_l3(sid, &shell, &path, &[]);
    let old_shell = until("the first shell", || shell_of(sid));
    let mut before = CodexPlugin::new();
    let host = OpsHost::new(plugin_state.clone(), Arc::new(crate::plugins::l3_test_support::NoPane));
    let ran = Bound { sid, thread: thread.into(), home: account.display().to_string(), cwd: Some(work.display().to_string()), model: Some("gpt-5.5".into()) };
    before.note_threads(&host, &now(), &[ran], &[sid].into());
    let saved = fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap();
    assert!(saved.contains(&format!("\t{thread}\t{}\t", old_shell.pid)), "{saved}");

    // the machine shuts down: the session is hung up and keeps its screen
    unsafe { libc::kill(first.child.id() as i32, libc::SIGHUP) };
    let state_bin = marspot_term::session_registry::session_dir(sid).join("state.bin");
    until("the hung-up session to save its screen", || state_bin.exists().then_some(()));
    drop(first);
    before.note_threads(&host, &now(), &[], &HashSet::new());
    assert!(fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap().contains(thread), "the row outlives the session process");

    // it comes back on the same id, from the saved screen, with a new shell
    let second = spawn_l3(sid, &shell, &path, &[]);
    let new_shell = until("the new shell", || shell_of(sid).filter(|s| *s != old_shell));
    until("the saved screen to be applied", || (!state_bin.exists()).then_some(()));

    // a new process of the shell starts: it has only what is on disk
    let host = OpsHost::new(plugin_state.clone(), Arc::new(Proxy::of(&second)));
    let mut plugin = CodexPlugin::new();
    plugin.cold.pace = Pace::own();
    plugin.load_last_bound(&host);
    let restored = until("codex to be put back", || {
        plugin.cold_tick(&host, &now(), &[], &HashSet::new());
        host.drive();
        codex_in(sid)
    });
    let argv = until("the stand-in's command line", || crate::plugins::pidtree::proc_cmdline(restored.pid).filter(|c| c.contains(thread)));
    assert!(argv.contains(&format!("codex resume {thread} -m gpt-5.5")), "{argv}");
    let codex_home = marspot::pidtree::proc_env_value(restored.pid, "CODEX_HOME");
    assert_eq!(codex_home.as_deref(), Some(account.to_str().unwrap()), "the home it ran under");
    let cwd = crate::plugins::pidtree::proc_cwd(restored.pid).map(|c| fs::canonicalize(c).unwrap());
    assert_eq!(cwd, Some(fs::canonicalize(&work).unwrap()), "in the directory it ran in");
    // the stand-in's app-server took the thread's writer lock: it loaded
    // the thread from its rollout
    let lock = account.join(format!("thread-writer-locks/{thread}.lock"));
    until("the thread to be loaded", || lock.exists().then_some(()));
    let logged = host.logged.lock().unwrap().clone();
    assert!(logged.iter().any(|(e, l)| e == "cold_restore.start" && l.contains(&format!("pane {sid}")) && l.contains("on P2")), "{logged:?}");
    let log = until("the launch line", || {
        host.drive();
        let t = crate::test_log::text();
        t.lines().any(|l| l.contains("launch.marspot") && l.contains("reason=cold_restore")).then_some(t)
    });
    let launch = log.lines().find(|l| l.contains("launch.marspot")).unwrap();
    for want in [format!("sid={sid}"), "agent=codex".into(), "account=P2".into(), format!("argv=codex_resume_{thread}_-m_gpt-5.5")] {
        assert!(launch.contains(&want), "{want} missing: {launch}");
    }
    assert_ne!(new_shell, old_shell);

    // a second pass of the same process does not type into the pane again
    for _ in 0..3 {
        plugin.cold.pace.set_last_start(None);
        plugin.cold_tick(&host, &now(), &[], &[sid].into());
        host.drive();
    }
    assert_eq!(host.sent.lock().unwrap().len(), 1, "one restore per pane");

    unsafe { libc::kill(restored.pid, libc::SIGTERM) };
    until("the restored codex to be gone", || codex_in(sid).is_none().then_some(()));
    stop_daemon(&account, &root.0);
    drop(second);
}
