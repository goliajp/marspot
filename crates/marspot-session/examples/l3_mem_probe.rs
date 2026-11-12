//! What a pane's session holds in memory: fresh and idle, at the peak
//! of a flood, and once the flood has ended and the pane is quiet.
//!
//! Starts N sessions the way a core does, lets each sit for a moment,
//! has all of them `cat` the same corpus at once, and reads each
//! process's physical footprint and resident size from the kernel
//! (`proc_pid_rusage`, what `footprint` and Activity Monitor show) every
//! 20 ms throughout.  The quiet reading is taken two seconds after the
//! last frame, when everything a flood borrowed should have been given
//! back -- people keep dozens of panes open, and what a pane keeps
//! after its one big `cat` is what they pay for all day.
//!
//!     cargo run --release -p marspot-session --example l3_mem_probe -- \
//!       target/release/marspot-session bench/scenarios/cat-mixed.bin 8 9

#[path = "support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::Frame;
use support::{Session, cleanup, die};

const COLS: u16 = 122;
const ROWS: u16 = 39;

/// Physical footprint and resident size of `pid`, in KiB.
fn memory_kib(pid: u32) -> Option<(u64, u64)> {
    // SAFETY: an all-zero rusage_info_v4 is a valid value for the
    // kernel to fill, and the cast is the C idiom for this call.
    let info = unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        );
        (rc == 0).then_some(info)?
    };
    Some((info.ri_phys_footprint / 1024, info.ri_resident_size / 1024))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        die("usage: l3_mem_probe <marspot-session> <scenario> <repeats> [panes]");
    }
    let bin = PathBuf::from(&args[1]);
    let scenario = std::fs::canonicalize(&args[2]).unwrap_or_else(|e| die(format!("{}: {e}", args[2])));
    let repeats: usize = args[3].parse().unwrap_or(1);
    let panes: usize = args.get(4).and_then(|n| n.parse().ok()).unwrap_or(1);

    let sandbox = std::env::temp_dir().join(format!("l3-mem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    struct Sandbox(PathBuf);
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _sandbox = Sandbox(sandbox.clone());
    std::fs::create_dir_all(sandbox.join("logs")).expect("mkdir sandbox/logs");
    std::fs::create_dir_all(sandbox.join("sessions")).expect("mkdir sandbox/sessions");
    let script = sandbox.join("pane.sh");
    let files = vec![scenario.to_string_lossy().into_owned(); repeats.max(1)].join(" ");
    // every pane starts its cat on the same wall-clock second
    std::fs::write(
        &script,
        format!("#!/bin/zsh\nwhile [[ ! -e {go} ]]; do sleep 0.01; done\n/bin/cat {files}\nexec sleep 600\n", go = sandbox.join("go").display()),
    )
    .expect("write the pane script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // SAFETY: single-threaded here; the sessions inherit both.
    unsafe {
        std::env::set_var("MARSPOT_STATE_DIR", &sandbox);
        std::env::set_var("SHELL", &script);
    }

    let mut sessions: Vec<Session> = (0..panes).map(|i| Session::spawn_with_id(&bin, COLS, ROWS, Some(i as u64 + 1))).collect();
    for s in &sessions {
        // a core reads what its session sends
        let mut pokes = s.control.try_clone().unwrap_or_else(|e| die(format!("clone the control stream: {e}")));
        std::thread::spawn(move || while let Ok(Some(_)) = Frame::read_from(&mut pokes) {});
    }
    for s in &sessions {
        s.wait_ready("session");
    }
    std::thread::sleep(Duration::from_millis(1500));
    let read_all = |sessions: &[Session]| -> Vec<(u64, u64)> {
        sessions.iter().map(|s| memory_kib(s.pid()).unwrap_or_else(|| die("a session is gone"))).collect()
    };
    let idle = read_all(&sessions);

    std::fs::write(sandbox.join("go"), b"").expect("start the flood");
    let mut peak = idle.clone();
    let seq = |sessions: &[Session]| sessions.iter().map(|s| s.reader.seq()).sum::<u64>();
    let (mut last_seq, mut last_change) = (seq(&sessions), Instant::now());
    let started = Instant::now();
    let mut flooded = false;
    loop {
        for (p, now) in peak.iter_mut().zip(read_all(&sessions)) {
            *p = (p.0.max(now.0), p.1.max(now.1));
        }
        let s = seq(&sessions);
        if s != last_seq {
            (last_seq, last_change) = (s, Instant::now());
            flooded = true;
        }
        if flooded && last_change.elapsed() >= Duration::from_secs(2) {
            break;
        }
        if started.elapsed() >= Duration::from_secs(120) {
            die("the flood never ended");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let quiet = read_all(&sessions);
    let flood_secs = (last_change - started).as_secs_f64();
    cleanup(&mut sessions);

    let line = |name: &str, v: &[(u64, u64)]| {
        let n = v.len() as u64;
        let (fp, rss): (u64, u64) = v.iter().fold((0, 0), |a, x| (a.0 + x.0, a.1 + x.1));
        println!("{name:11} footprint {:>6} KiB a pane, {:>7} KiB all; resident {:>6} KiB a pane, {:>7} KiB all", fp / n, fp, rss / n, rss);
    };
    println!("{panes} pane(s), flood of {:.2} s", flood_secs);
    line("idle", &idle);
    line("flood peak", &peak);
    line("quiet after", &quiet);
}
