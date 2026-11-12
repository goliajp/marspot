//! Production-path bulk-cat throughput: shell → core → **L3**.
//!
//! `bin/measure.sh` drives `mcli`, where the parser and the renderer
//! share one process and one thread.  That is not the architecture the
//! product ships: since 2026-06-13 every pane is its own
//! `marspot-session` (L3) that owns the pty and publishes into shm,
//! and the number a user actually experiences comes from that path.
//!
//! This probe measures it headlessly: spawn one L3 in a sandbox the
//! way a core does -- its grid region and control socket inherited --
//! hand it a trial script through `SHELL`, and read the elapsed time
//! the script took off zsh's `$EPOCHREALTIME` around the trial -- the
//! clock and the commands `bin/_lib.sh` gives every other terminal.
//! This used `/usr/bin/time -p`, whose two decimals are a 2 % step on a
//! trial of half a second.
//!
//! The trial ends with a DSR round-trip (`CSI 6 n`) for the same reason
//! every other live measurement here does: `cat` returning proves the
//! bytes reached the pty, not that the terminal consumed them.  L3 owns
//! the pty and answers capability queries itself, so the reply is that
//! session certifying it drained the corpus.
//!
//! This replaces a probe of the same name that was deleted with the L4
//! shelld in RFC-003 Phase 6g (888bf1b, 2026-06-17).  `bin/measure-l3.sh`
//! and the `--full` gate kept calling it; with the source gone they ran
//! a stale binary that failed every trial, and
//! `bench/results/l3-throughput.json` sat frozen at its last success
//! (2026-06-14) looking exactly like fresh data.
//!
//! Usage (the shape `bin/measure-l3.sh` expects):
//!   l3_throughput <session-bin> <scenario-path> <repeats>
//! Prints elapsed nanoseconds on stdout, or nothing on failure.

use std::env;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use marspot_term::grid_shm::create_region;
use marspot_term::shell_proto::Frame;

/// The pane the trial runs in.
const COLS: u16 = 122;
const ROWS: u16 = 39;

/// The session this probe spawned, for `die` to take down with it.
static SESSION_PID: AtomicU32 = AtomicU32::new(0);

/// The trial's directory, for `die` to take with it too: a failed trial
/// used to leave its own behind, a third of a gigabyte of scrollback
/// each, and 25 of them were found on the runner one evening.
static SANDBOX: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn die(msg: &str) -> ! {
    eprintln!("FAIL: {msg}");
    // `process::exit` skips Drop, so the ChildGuard that would have
    // killed the session never runs.  Every failed trial used to leave a
    // live session holding a pty and spinning, and the harness only swept
    // orphans after the NEXT failure -- so one failure quietly taxed
    // every trial that followed it.  That read as the product needing
    // 30-60 s and twice the instructions for a corpus it normally
    // absorbs in 1.3 s.
    let pid = SESSION_PID.load(Ordering::Relaxed);
    if pid != 0 {
        // SAFETY: a kill to a pid this process spawned; a stale pid can
        // only fail with ESRCH, which is ignored.
        unsafe { libc::kill(pid as libc::c_int, libc::SIGKILL) };
        // its writer threads must be gone before its files are
        let mut status = 0;
        // SAFETY: reaps the child this process spawned; ECHILD is ignored.
        unsafe { libc::waitpid(pid as libc::c_int, &mut status, 0) };
    }
    if let Some(sandbox) = SANDBOX.get() {
        let _ = std::fs::remove_dir_all(sandbox);
    }
    std::process::exit(1)
}

/// What the session spent: user and system CPU nanoseconds, wakeups, and
/// the hardware counters that separate "did more work" from "ran the same
/// work on a slower core".
///
/// Wall clock on a shared host times the host's other tenants as much
/// as the parser: preemption adds elapsed time without adding work.
/// CPU time does not move when the scheduler hands the core to someone
/// else, so it says what the code costs rather than what the neighbours
/// did.  Reported alongside wall, never instead of it — a change that
/// burns the same cycles over a longer window is still a regression the
/// user feels.
fn session_cpu_ns(pid: u32) -> Option<Spend> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, correctly-sized rusage_info_v4, which is
    // what RUSAGE_INFO_V4 tells the kernel to fill; the cast matches the
    // C idiom (`(rusage_info_t *)&ru`).
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    };
    if rc != 0 {
        return None;
    }
    // Mach ticks, NOT nanoseconds, whatever the field name suggests.
    // Calibrated against a child that burned a known 2.000 s of CPU
    // (getrusage agreed): the raw total read 48_011_091, which is 2.000 s
    // only after the timebase.  Taken as nanoseconds it would have
    // reported this path absorbing bytes 14× faster than the parser can
    // actually run.
    // Declared here rather than taken from libc, which deprecates its
    // copy in favour of a crate: two fields and one libSystem symbol are
    // not worth a dependency.
    #[repr(C)]
    struct MachTimebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    }
    let mut tb = MachTimebase { numer: 0, denom: 0 };
    // SAFETY: `tb` is a live MachTimebase, matching the C struct, for
    // the kernel to fill.
    if unsafe { mach_timebase_info(&mut tb) } != 0 || tb.denom == 0 {
        return None;
    }
    // Kept apart, not summed.  The two answer different questions: user
    // time is the parsing work, which the same corpus has to cost
    // whatever else the host is doing, while system time is syscall
    // overhead that genuinely falls when preemption lets bytes pool in
    // the pty and each read returns a bigger chunk.  Summed, that second
    // effect moves the total by ~48 % between runs and hides the first.
    let ns = |ticks: u64| (ticks as u128 * tb.numer as u128 / tb.denom as u128) as u64;
    // Wakeups are the test for why CPU time moves between runs: if a
    // starved session does less work per byte because bytes pool in the
    // pty and each round handles a bigger chunk, then CPU and wakeups
    // fall together.  If CPU moves while wakeups hold, that story is
    // wrong and the cost went somewhere else.
    let wakeups = info.ri_interrupt_wkups + info.ri_pkg_idle_wkups;
    Some(Spend {
        user_ns: ns(info.ri_user_time),
        sys_ns: ns(info.ri_system_time),
        wakeups,
        // Instructions retired is the only one of these that answers
        // "did this build do more work": it does not move when the
        // scheduler puts the thread on an efficiency core or drops the
        // clock, while every time-based reading does.  Cycles come with
        // it so the two can be divided — same instructions at worse IPC
        // is a memory-system story, not a code-size one.
        instructions: info.ri_instructions,
        cycles: info.ri_cycles,
    })
}

/// What one trial cost, in every unit the kernel will report.
struct Spend {
    user_ns: u64,
    sys_ns: u64,
    wakeups: u64,
    instructions: u64,
    cycles: u64,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        die("usage: l3_throughput <session-bin> <scenario-path> <repeats>");
    }
    let session_bin = PathBuf::from(&args[1]);
    let scenario = match std::fs::canonicalize(&args[2]) {
        Ok(p) => p,
        Err(e) => die(&format!("resolve {}: {e}", args[2])),
    };
    let repeats: usize = args[3].parse().unwrap_or(1);
    if !session_bin.exists() {
        die(&format!("marspot-session missing at {session_bin:?}"));
    }

    let pid = std::process::id();
    let sandbox = env::temp_dir().join(format!("l3-throughput-{pid}"));
    let _ = std::fs::remove_dir_all(&sandbox);
    let _ = SANDBOX.set(sandbox.clone());
    std::fs::create_dir_all(sandbox.join("logs")).expect("mkdir sandbox/logs");
    std::fs::create_dir_all(sandbox.join("sessions")).expect("mkdir sandbox/sessions");

    // The trial script, byte-identical in shape to the one bin/_lib.sh
    // writes for every other terminal: cat the corpus `repeats` times,
    // then make the terminal certify it consumed the lot.
    //
    // No pause before the clock starts, though a session is still
    // finishing its own start when its program begins, and in some
    // trials the first chunks wait 3 to 12 ms for its loop.  A pause
    // costs more than that and not to the session: after 0.3 s of
    // nothing the machine takes 20 ms to start the trial's shell where
    // it took 4, and the same `cat` runs 6 % slower.  The other
    // terminals' trials follow each other without one either.
    let marker = sandbox.join("marker.txt");
    let script = sandbox.join("trial.sh");
    {
        let mut f = std::fs::File::create(&script).expect("create trial script");
        let arg = scenario.to_string_lossy();
        let cats = std::iter::repeat_n(arg.as_ref(), repeats.max(1))
            .collect::<Vec<_>>()
            .join(" ");
        write!(
            f,
            "#!/bin/zsh\nzmodload zsh/datetime\nexec 2> {marker}\n__s=$EPOCHREALTIME\n/bin/zsh -c '\n  /usr/bin/time -l /bin/cat {cats}\n  stty raw -echo 2>/dev/null\n  printf \"\\033[6n\" > /dev/tty\n  IFS=\"[;\" read -t 5 -srd R _ _row _col < /dev/tty\n  stty sane 2>/dev/null\n  printf \"cpr=%s,%s\\n\" \"$_row\" \"$_col\" >&2\n'\n__e=$EPOCHREALTIME\nprintf 'real %.6f\\n' $((__e - __s)) >&2\n",
            marker = marker.display(),
            cats = cats,
        )
        .expect("write trial script");
    }
    let mut perms = std::fs::metadata(&script).expect("stat script").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&script, perms).expect("chmod script");

    // Started the way a core starts a pane: the grid's shared region,
    // already at the size of the pane, and the control socket are both
    // inherited, so the session is 122 by 39 before its program runs.
    //
    // This probe used to start the session with neither and send the
    // size over the session's listener once it could connect.  A
    // session started like that opens its pty at 80 by 24 at once, the
    // trial was a few milliseconds into its `cat` when the size
    // arrived, and every trial paid for one reflow of the megabyte or
    // two of history it had by then: 6 ms of a 450 ms trial with
    // history in memory, 12 to 31 ms with history in files.  No pane a
    // core starts does that.
    let session_id: u64 = 1;
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(&format!("create_region: {e}")));
    let (control, child_end) = UnixStream::pair().unwrap_or_else(|e| die(&format!("socket pair: {e}")));
    for fd in [region.as_raw_fd(), child_end.as_raw_fd()] {
        // SAFETY: fcntl on two fds this process just made; the child
        // inherits them by number.
        let cleared = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0
        };
        if !cleared {
            die(&format!("clear CLOEXEC on fd {fd}: {}", std::io::Error::last_os_error()));
        }
    }
    let child = Command::new(&session_bin)
        .env("MARSPOT_STATE_DIR", &sandbox)
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        .env("MARSPOT_SHELL_CONTROL_FD", child_end.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        // `SHELL`, not `MARSPOT_SHELL`: an L3 spawns `$SHELL` directly
        // (local_session.rs) and never consults the MARSPOT_SHELL
        // override that `marspot_term::session::Session` — the path
        // mcli and the L2 in-process backend take — honours.  Pointing
        // MARSPOT_SHELL at the trial script leaves the session sitting
        // in an interactive zsh waiting for input, which looks exactly
        // like a slow terminal until the probe times out.
        .env("SHELL", &script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(&format!("spawn marspot-session: {e}")));
    drop(child_end);
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let session_pid = child.id();
    SESSION_PID.store(session_pid, Ordering::Relaxed);
    // A seam for checking that a failed trial takes its session with it.
    // `spawn` fails before the session is up, `connected` after it holds a
    // pty and is absorbing -- the moment the real die sites (missing
    // DSR) happen, and the only one where an orphan would have
    // anything left to do.
    let die_at = env::var("MARSPOT_PROBE_DIE_AT").unwrap_or_default();
    if die_at == "spawn" {
        die("asked to die after spawn");
    }
    let _guard = ChildGuard(child);

    // Keep reading the control socket, or this probe measures its own
    // backpressure.  An L3 pushes frames at its core (GridReady and
    // friends); a client that never reads fills the socket buffer.
    // The first version of this probe did exactly that and reported
    // the production path at 26 MB/s with 75 % of the session's
    // samples in `write`, which reads like a damning finding about L3
    // and was a bug in the harness.  A real core drains this socket;
    // so does this.
    let mut rx = control.try_clone().unwrap_or_else(|e| die(&format!("try_clone control stream: {e}")));
    thread::spawn(move || while let Ok(Some(_frame)) = Frame::read_from(&mut rx) {});

    if die_at == "connected" {
        die("asked to die once connected");
    }

    // The session runs the script once its pty is up; wait for the
    // marker to carry a complete `time` record.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut text = String::new();
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&marker)
            && s.lines().any(|l| l.starts_with("real ")) {
                text = s;
                break;
            }
        thread::sleep(Duration::from_millis(20));
    }
    if text.is_empty() {
        die("trial produced no timing");
    }
    if !text.lines().any(|l| l.starts_with("cpr=") && l[4..5].chars().next().is_some_and(|c| c.is_ascii_digit())) {
        die("no DSR reply — the session never confirmed it consumed the corpus");
    }
    // what the writing side spent: `time -l` around the `cat` alone
    let rusage = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_suffix(name).map(|v| v.trim().to_string()))
            .unwrap_or_else(|| "-".into())
    };
    // `0.45 real  0.00 user  0.30 sys`, each figure before its name
    let clock = |name: &str| {
        text.lines()
            .filter(|l| l.contains(" real") && l.contains(" sys"))
            .find_map(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                words.iter().position(|w| *w == name).and_then(|i| i.checked_sub(1)).map(|i| words[i].to_string())
            })
            .unwrap_or_else(|| "-".into())
    };
    eprintln!(
        "writer: user_s={} sys_s={} vcsw={} ivcsw={}",
        clock("user"),
        clock("sys"),
        rusage("voluntary context switches"),
        rusage("involuntary context switches"),
    );
    let secs = text
        .lines()
        .find_map(|l| l.strip_prefix("real "))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or_else(|| die("could not parse `real` from the trial marker"));
    // Corroborate with something the timing cannot fake: how much the
    // session actually wrote to disk this run.  A throughput number
    // whose explanation is "the scrollback write" is only worth
    // believing if the file grew.
    let sess_dir = sandbox.join("sessions").join(session_id.to_string());
    let sz = |name: &str| {
        std::fs::metadata(sess_dir.join(name))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    eprintln!(
        "witness: scrollback.bin={} bytelog={} state.bin={}",
        sz("scrollback.bin"),
        sz("bytelog"),
        sz("state.bin")
    );
    // `-` when the kernel would not say (the session died early): the
    // caller has to see a missing reading as missing, not as zero cost.
    let wall_ns = (secs * 1e9) as u64;
    match session_cpu_ns(session_pid) {
        Some(spend) => {
            let (user, sys) = (spend.user_ns, spend.sys_ns);
            let cpu = user + sys;
            // A gauge that cannot fail is not a gauge.  Absorbing this
            // corpus keeps at least one core busy for most of the
            // window, so a CPU reading far below the wall time means the
            // unit conversion broke, not that the work got free — the
            // exact mistake a tick/nanosecond mixup makes, and it reads
            // as a spectacular improvement rather than as an error.
            if cpu * 8 < wall_ns {
                die(&format!(
                    "cpu {cpu} ns against wall {wall_ns} ns — too little CPU \
                     for this much work; the timebase conversion is wrong"
                ));
            }
            println!(
                "{wall_ns} {user} {sys} {} {} {}",
                spend.wakeups, spend.instructions, spend.cycles
            );
        }
        None => println!("{wall_ns} - - - - -"),
    }
    let _ = std::fs::remove_dir_all(&sandbox);
}
