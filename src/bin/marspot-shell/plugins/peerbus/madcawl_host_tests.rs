//! The madcawl host against a stand-in `madcawl-shell`: a perl script
//! that accepts on fd 3, answers one line per connection, leaves after
//! `--exit-idle` seconds with no connection, and crashes or ignores
//! SIGTERM when a file in its working directory, the rig's home, says
//! so.  Real processes, real sockets.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_term::test_env::{EnvGuard, TempDir};

use super::madcawl_host::{Config, Host};
use super::madcawl_host_proc as proc;
use super::mcp_servers;

const STAND_IN: &str = r#"#!/usr/bin/perl
use strict;
use Cwd;
my $h = getcwd();
open(my $a, '>>', "$h/argv"); print $a join(' ', @ARGV), "\n"; close $a;
open(my $p, '>>', "$h/pids"); print $p "$$\n"; close $p;
exit 3 if -e "$h/crash";
$SIG{TERM} = 'IGNORE' if -e "$h/ignore-term";
my %opt = @ARGV[0 .. 5];
open(my $l, '<&=', 3) or die "fd 3: $!";
while (1) {
    my $rin = ''; vec($rin, 3, 1) = 1;
    my $n = select(my $rout = $rin, undef, undef, $opt{'--exit-idle'});
    exit 0 if $n == 0;
    next if $n < 0;
    accept(my $c, $l) or die "accept: $!";
    my $line = <$c>;
    syswrite($c, "served $$\n");
    close $c;
}
"#;

/// A home with the stand-in installed and a short state directory
/// (a socket path has to fit in 104 bytes).
struct Rig {
    home: TempDir,
    state: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let home = TempDir::new("madcawl-host");
        let bin = home.join(mcp_servers::MADCAWL_SHELL_BIN);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, STAND_IN).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = PathBuf::from(format!("/tmp/mch-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&state).unwrap();
        Rig { home, state }
    }

    fn cfg(&self) -> Config {
        Config {
            shell: self.home.join(mcp_servers::MADCAWL_SHELL_BIN),
            socket: self.state.join("madcawl.sock"),
            pid_file: self.state.join("madcawl-host.pid"),
            host_log: self.state.join("logs/madcawl-host.log"),
            audit: self.state.join("logs/madcawl-audit.log"),
            home: self.home.path().to_path_buf(),
            idle_secs: 1,
            quick: Duration::ZERO,
        }
    }

    fn pids(&self) -> Vec<i32> {
        let text = std::fs::read_to_string(self.home.join("pids")).unwrap_or_default();
        text.lines().map(|l| l.parse().unwrap()).collect()
    }

    fn flag(&self, name: &str) {
        std::fs::write(self.home.join(name), "").unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

fn started(cfg: &Config) -> Host {
    let mut h = Host::default();
    h.apply(cfg.clone(), true);
    assert!(h.bound(), "bound");
    h
}

/// One request on the socket; the pid of the host that answered.
fn call(socket: &Path) -> i32 {
    let mut c = UnixStream::connect(socket).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(b"hi\n").unwrap();
    let mut line = String::new();
    BufReader::new(&c).read_line(&mut line).unwrap();
    line.trim().strip_prefix("served ").unwrap_or_else(|| panic!("answer: {line:?}")).parse().unwrap()
}

fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only checks
    unsafe { libc::kill(pid, 0) != 0 }
}

fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn madcawl_host_starts_on_first_connect_and_again_after_idle_exit() {
    let rig = Rig::new();
    let cfg = rig.cfg();
    let _h = started(&cfg);
    std::thread::sleep(Duration::from_millis(300));
    assert!(rig.pids().is_empty(), "no host before the first connection");
    let mode = std::fs::metadata(&cfg.socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let first = call(&cfg.socket);
    assert_eq!(call(&cfg.socket), first, "one host serves both calls");
    let argv = std::fs::read_to_string(rig.home.join("argv")).unwrap();
    let want = format!(
        "--serve {} --listen-fd 3 --exit-idle 1 --allow {} --audit {}\n",
        cfg.socket.display(),
        cfg.home.display(),
        cfg.audit.display()
    );
    assert_eq!(argv, want);
    let pid_file = std::fs::read_to_string(&cfg.pid_file).unwrap();
    assert!(pid_file.starts_with(&format!("{first} ")), "{pid_file}");

    until("the idle host leaves and is reaped", 10, || gone(first));
    until("the pid file goes with it", 5, || !cfg.pid_file.exists());
    let second = call(&cfg.socket);
    assert_ne!(second, first);
    assert_eq!(rig.pids(), vec![first, second]);
}

#[test]
fn madcawl_host_crash_brake_gives_up_after_five_and_unlinks() {
    let rig = Rig::new();
    rig.flag("crash");
    let cfg = rig.cfg();
    let mut h = started(&cfg);
    let _waiting = UnixStream::connect(&cfg.socket).unwrap();
    until("the brake gives up", 20, || !h.bound());
    assert_eq!(rig.pids().len(), 5);
    assert!(!cfg.socket.exists(), "the socket is unlinked");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.pids().len(), 5, "and no host is started after");

    std::fs::remove_file(rig.home.join("crash")).unwrap();
    h.apply(cfg.clone(), true);
    assert!(h.bound(), "the next pass starts from zero");
    call(&cfg.socket);
}

#[test]
fn madcawl_host_stop_ends_the_host_and_removes_the_socket() {
    let rig = Rig::new();
    let cfg = rig.cfg();
    let mut h = started(&cfg);
    let pid = call(&cfg.socket);
    h.stop();
    assert!(gone(pid), "ended and reaped");
    assert!(!cfg.socket.exists() && !cfg.pid_file.exists());

    // one that ignores SIGTERM is killed after the grace period
    rig.flag("ignore-term");
    let cfg = Config { idle_secs: 30, ..cfg };
    let mut h = started(&cfg);
    let pid = call(&cfg.socket);
    let t = Instant::now();
    h.stop();
    assert!(gone(pid));
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_secs(5), "{took:?}");
    assert!(!cfg.socket.exists());

    // and a pass that finds the programs gone closes the socket
    let mut h = started(&cfg);
    h.apply(cfg.clone(), false);
    assert!(!h.bound() && !cfg.socket.exists());
}

#[test]
fn madcawl_host_replaces_a_stale_socket_file() {
    let rig = Rig::new();
    let cfg = rig.cfg();
    drop(UnixListener::bind(&cfg.socket).unwrap());
    assert!(UnixStream::connect(&cfg.socket).is_err(), "stale");
    let h = started(&cfg);
    call(&cfg.socket);
    drop(h);
    // and a plain file left at the path
    std::fs::write(&cfg.socket, "x").unwrap();
    let _h = started(&cfg);
    call(&cfg.socket);
}

#[test]
fn madcawl_host_ends_the_host_a_previous_image_left() {
    let rig = Rig::new();
    let cfg = rig.cfg();
    let child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    drop(child);
    until("the child is up", 5, || marspot::pidtree::proc_row(pid).is_some());
    proc::write_pid_file(&cfg.pid_file, pid);
    let _h = started(&cfg);
    assert!(gone(pid), "ended");
    let mut st = 0;
    // SAFETY: a non-blocking look
    assert_eq!(unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) }, -1, "and reaped");
    assert!(!cfg.pid_file.exists());

    // a pid that is not our child is left alone
    let parent = std::os::unix::process::parent_id() as i32;
    std::fs::write(&cfg.pid_file, format!("{parent} 0\n")).unwrap();
    let mut h = Host::default();
    h.apply(cfg.clone(), false);
    assert!(!cfg.pid_file.exists() && !gone(parent));
}

/// The previous image's host had exited and the new image's start
/// waited for it with the other inherited children: its pid may name
/// another process by now, so the host's start takes it from there and
/// neither waits for nor signals it.
#[test]
fn madcawl_host_leaves_a_previous_host_the_inherited_children_took() {
    let rig = Rig::new();
    let cfg = rig.cfg();
    let child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id() as i32;
    drop(child);
    until("the child has exited", 5, || {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: a look at one child of ours that leaves it unwaited
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        r == 0 && info.si_pid == pid
    });
    crate::inherited::adopt();
    std::fs::write(&cfg.pid_file, format!("{pid} 0\n")).unwrap();
    proc::end_previous(&cfg.pid_file);
    assert!(!cfg.pid_file.exists());
    assert!(!crate::inherited::disown(pid), "the host's start took it out of the inherited ones");
}

#[test]
fn madcawl_host_binds_only_with_both_programs_installed() {
    let home = TempDir::new("madcawl-installed");
    let mut env = EnvGuard::new();
    env.set("HOME", home.path());
    assert!(!mcp_servers::madcawl_installed(home.path()));
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("madcawl-shell"), "").unwrap();
    assert!(!mcp_servers::madcawl_installed(home.path()), "the host alone");
    std::fs::create_dir(bin.join("madcawl-mcp")).unwrap();
    assert!(!mcp_servers::madcawl_installed(home.path()), "a directory is not the program");
    std::fs::remove_dir(bin.join("madcawl-mcp")).unwrap();
    std::fs::write(bin.join("madcawl-mcp"), "").unwrap();
    assert!(mcp_servers::madcawl_installed(home.path()));
    assert_eq!(mcp_servers::madcawl_paths(), vec![bin.join("madcawl-mcp"), bin.join("madcawl-shell")]);
}
