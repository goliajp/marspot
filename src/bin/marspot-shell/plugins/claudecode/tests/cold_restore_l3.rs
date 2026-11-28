//! A cold restore against real session processes: a pane's session
//! process is hung up the way a shutting-down machine does it, a new
//! one comes back on the same id with the saved screen and a new shell,
//! and claude is put back in it.  A stand-in program named `claude`
//! records what it was started with.

use super::*;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Instant;

use marspot_term::shell_proto::{Frame, MsgType, encode_inject_input, encode_pane_hold_grid};

/// A state root with a short path: a session binds its socket under it
/// and macOS caps a socket path at 104 bytes.
struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One session process, killed and reaped when dropped.
struct L3 {
    child: std::process::Child,
    control: UnixStream,
}

impl Drop for L3 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn no_cloexec(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
}

/// Session `sid` on the built session binary, its shell `shell`.
fn spawn_l3(sid: u64, shell: &Path, path: &str) -> L3 {
    let region = marspot_term::grid_shm::create_region(80, 24).unwrap();
    let (control, child_end) = UnixStream::pair().unwrap();
    no_cloexec(region.as_raw_fd());
    no_cloexec(child_end.as_raw_fd());
    let child = std::process::Command::new(marspot_term::test_env::built_binary("marspot-session"))
        .env("MARSPOT_SHELL_CONTROL_FD", child_end.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", sid.to_string())
        .env("SHELL", shell)
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // what the session says on its socket is not read here; left unread
    // it would fill the socket and stall the session
    let mut drain = control.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while drain.read(&mut buf).is_ok_and(|n| n > 0) {}
    });
    L3 { child, control }
}

/// Typing into the session the way the core forwards it.
struct Proxy(Mutex<UnixStream>);

impl Proxy {
    fn frame(&self, t: MsgType, payload: Vec<u8>) -> std::io::Result<()> {
        Frame::new(t, payload).write_to(&mut *self.0.lock().unwrap()).map(|_| ())
    }
}

impl InjectInputProxy for Proxy {
    fn inject_input(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.frame(MsgType::InjectInput, encode_inject_input(sid, bytes))
    }
    fn hold_grid(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(sid, on))
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        self.inject_input(sid, text.as_bytes())
    }
    fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    eprintln!("waiting for {what}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Start what was queued and step what runs, as the supervisor does.
fn drive(host: &FakeHost, sid: u64) {
    host.pump_ops();
    let pane = FakePaneSessionHost { sid };
    for s in host.sessions.lock().unwrap().iter_mut() {
        s.on_tick(&pane);
    }
}

fn shell_of(sid: u64) -> Option<pidtree::ProcId> {
    now().session(sid).and_then(|s| s.shell)
}

fn now() -> Arc<crate::observe::Observation> {
    crate::observe::shared().fresh(crate::observe::NOW)
}

fn claude_in(sid: u64) -> Option<pidtree::ProcId> {
    let obs = crate::observe::shared().fresh(crate::observe::NOW);
    obs.session(sid)?.find("claude").map(|p| p.row.id())
}

fn bound(sid: u64, claude: pidtree::ProcId, profile: u8, home: &Path, uuid: &str) -> ScanResult {
    let mut scan = scan_of(sid, CcActivity::AwaitingUser);
    let meta = BindMeta {
        profile_num: profile,
        config_dir: Some(home.join(format!(".claude-profile-{profile}")).to_string_lossy().into_owned()),
        claude,
        ..pane_on_p1(home, uuid)
    };
    scan.new_meta.insert(sid, meta);
    scan.choices.insert(sid, CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("high".into()) });
    scan
}

/// The stand-in: writes its account and arguments, draws a screenful,
/// then lives on as a process whose argv[0] is `claude`.
fn stand_in(bin: &Path, out: &Path) {
    let script = format!(
        "#!/bin/bash\n{{ echo ---; printf '%s\\n' \"${{CLAUDE_CONFIG_DIR:-}}\" \"$@\"; }} >> '{}'\nhead -c 4096 /dev/zero | tr '\\0' x\nexec -a claude /bin/sleep 600\n",
        out.display()
    );
    fs::write(bin.join("claude"), script).unwrap();
    fs::set_permissions(bin.join("claude"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn a_restarted_session_gets_claude_back_and_a_claude_started_by_hand_is_observed() {
    if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
        eprintln!("SKIPPED: needs real session processes — set MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)");
        return;
    }
    let root = Root(PathBuf::from(format!("/tmp/mscr-{}", std::process::id())));
    let _ = fs::remove_dir_all(&root.0);
    let (bin, home, state) = (root.0.join("bin"), root.0.join("home"), root.0.join("state"));
    for d in [&bin, &home.join(".claude-profile-3"), &home.join(".claude-profile-6"), &state.join("plugins/claudecode")] {
        fs::create_dir_all(d).unwrap();
    }
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", &state).set("HOME", &home);
    crate::test_log::open();
    let argv_file = root.0.join("argv");
    stand_in(&bin, &argv_file);
    let shell = marspot_term::test_env::shell_without_rc_files(&root.0, "/bin/zsh");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // shm names are one namespace for the login: an id of this process's own
    let sid = u64::from(std::process::id()) * 100_000 + 1;
    let uuid = "5860b61a-0000-4000-8000-000000000483";
    let plugin_state = state.join("plugins/claudecode");

    // before the restart: claude ran in the pane, and the scan saw it
    let first = spawn_l3(sid, &shell, &path);
    let old_shell = until("the first shell", || shell_of(sid));
    let mut before = ClaudecodePlugin::new();
    let host = FakeHost::new(plugin_state.clone());
    before.note_bindings(&host, &bound(sid, live_claude(), 3, &home, uuid), &now());
    let saved = fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap();
    assert!(saved.contains(&format!("\t{}\t", old_shell.pid)), "{saved}");

    // the machine shuts down: the session is hung up and keeps its screen
    unsafe { libc::kill(first.child.id() as i32, libc::SIGHUP) };
    let state_bin = marspot_term::session_registry::session_dir(sid).join("state.bin");
    until("the hung-up session to save its screen", || state_bin.exists().then_some(()));
    drop(first);

    // the session process is gone before this one: its registry entry
    // stays, so the pane is not taken for closed and its row stays
    let gone = now();
    assert!(gone.session(sid).is_some_and(|s| !s.live), "the entry outlives its process");
    before.note_bindings(&host, &scan_of(sid, CcActivity::Absent), &gone);
    assert!(fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap().contains(uuid), "the row is kept");

    // the core starting it again removes its entry first, and the new
    // session process writes it back: a scan in between keeps the row
    let entry = marspot_term::session_registry::session_entry_path(sid);
    fs::remove_file(&entry).unwrap();
    let reviving = until("an observation without the entry", || {
        let o = now();
        o.session(sid).is_none().then_some(o)
    });
    before.note_bindings(&host, &scan_of(sid, CcActivity::Absent), &reviving);
    assert!(fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap().contains(uuid), "the row outlives the revival");

    // it comes back on the same id, from the saved screen, with a new shell
    let second = spawn_l3(sid, &shell, &path);
    let new_shell = until("the new shell", || shell_of(sid).filter(|s| *s != old_shell));
    until("the saved screen to be applied", || (!state_bin.exists()).then_some(()));

    // a new process of the shell starts: it has only what is on disk
    let proxy = Arc::new(Proxy(Mutex::new(second.control.try_clone().unwrap())));
    let client = Arc::new(ShelldClient::new(Some(proxy.clone() as Arc<dyn InjectInputProxy>)));
    let host = FakeHost::with_io(plugin_state.clone(), client.clone() as Arc<dyn pty_op::PtyIo>);
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(client.clone());
    plugin.load_last_bound(&host);
    let mut idle = scan_of(sid, CcActivity::Absent);
    idle.new_models.clear();

    // a copy of the shell's state whose pane was typed into since its
    // shell started: nothing is typed, and the row goes
    let typed_state = root.0.join("typed");
    fs::create_dir_all(&typed_state).unwrap();
    fs::copy(plugin_state.join("last_bound.tsv"), typed_state.join("last_bound.tsv")).unwrap();
    let typed_host = FakeHost::with_io(typed_state.clone(), client.clone() as Arc<dyn pty_op::PtyIo>);
    let mut typed = ClaudecodePlugin::new();
    typed.shelld = Some(client.clone());
    typed.cold.last_input = |_| Some(std::time::SystemTime::now());
    typed.load_last_bound(&typed_host);
    for _ in 0..5 {
        typed.cold_tick(&typed_host, &idle);
        drive(&typed_host, sid);
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(typed_host.submitted.lock().unwrap().is_empty(), "no restore into a pane typed into");
    assert!(!fs::read_to_string(typed_state.join("last_bound.tsv")).unwrap().contains(uuid), "its row is dropped");
    assert!(claude_in(sid).is_none());

    let t0 = Instant::now();
    let restored = until("claude to be put back", || {
        plugin.cold_tick(&host, &idle);
        drive(&host, sid);
        let found = claude_in(sid);
        if found.is_none() && t0.elapsed() > Duration::from_secs(13) {
            let tail = marspot_term::bytelog::tail(&marspot_term::paths::sessions_dir().join(sid.to_string()), 600).unwrap_or_default();
            eprintln!("logged: {:?}\nsent: {:?}\npane: {:?}", host.logged.lock().unwrap(), host.sent.lock().unwrap(), String::from_utf8_lossy(&tail));
        }
        found
    });
    let argv = until("the stand-in's record", || fs::read_to_string(&argv_file).ok().filter(|t| t.contains(uuid)));
    let lines: Vec<&str> = argv.lines().skip(1).collect();
    assert_eq!(lines[0], home.join(".claude-profile-3").to_string_lossy(), "the account it ran on: {argv}");
    let args = lines[1..].join(" ");
    assert!(args.contains(&format!("--resume {uuid} --model claude-opus-5-5 --effort high")), "{args}");
    let logged = host.logged.lock().unwrap().clone();
    assert!(logged.iter().any(|(e, l)| e == "cold_restore.start" && l.contains(&format!("pane {sid}")) && l.contains("on P3")), "{logged:?}");
    let log = until("the launch line", || {
        drive(&host, sid);
        let t = crate::test_log::text();
        t.lines().any(|l| l.contains("launch.marspot") && l.contains("reason=cold_restore")).then_some(t)
    });
    let launch = log.lines().find(|l| l.contains("launch.marspot")).unwrap();
    for want in [format!("sid={sid}"), "agent=claude".into(), "account=P3".into(), format!("argv=claude_--resume_{uuid}_--model_claude-opus-5-5_--effort_high")] {
        assert!(launch.contains(&want), "{want} missing: {launch}");
    }
    assert_ne!(new_shell, old_shell);

    // the claude marspot started is not reported as someone else's
    plugin.cold_tick(&host, &bound(sid, restored, 3, &home, uuid));
    assert!(!crate::test_log::text().contains("launch.observed"), "marspot's own launch");

    // the person quits it and starts one by hand on another account
    unsafe { libc::kill(restored.pid, libc::SIGKILL) };
    until("the restored claude to be gone", || claude_in(sid).is_none().then_some(()));
    let by_hand = format!("CLAUDE_CONFIG_DIR={} claude\r", home.join(".claude-profile-6").display());
    proxy.inject_input(sid, by_hand.as_bytes()).unwrap();
    let typed = until("the claude started by hand", || claude_in(sid).filter(|c| *c != restored));
    plugin.cold_tick(&host, &bound(sid, typed, 6, &home, uuid));
    let observed = until("the observed line", || crate::test_log::text().lines().find(|l| l.contains("launch.observed")).map(str::to_string));
    for want in [format!("sid={sid}"), "agent=claude".into(), format!("pid={}", typed.pid), "account=P6".into(), "source=shell".into()] {
        assert!(observed.contains(&want), "{want} missing: {observed}");
    }
    unsafe { libc::kill(typed.pid, libc::SIGKILL) };
    drop(second);

    // a pane the app closed is marked so before its directory goes: the
    // row goes with the mark
    marspot_term::session_registry::mark_closed(sid).unwrap();
    plugin.note_bindings(&host, &scan_of(sid, CcActivity::Absent), &now());
    assert!(!fs::read_to_string(plugin_state.join("last_bound.tsv")).unwrap().contains(uuid), "a closed pane's row goes");
}
