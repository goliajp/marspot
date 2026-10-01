//! What N idle per-pane sessions cost: resident memory and CPU.
//!
//! Replaces `l3_rss_scaling`, whose source did not survive the repository
//! history rebuild. `bin/soak-l3-rss-scaling.sh` went on referring to it,
//! swallowed the build failure in a pipe, found a binary from June still
//! sitting in `target/`, and failed at run time with "N=1 probe failed".
//! For four months a document said a red line was "gated by" a gate that
//! could not run -- and if the wire protocol had happened not to change,
//! it would have been measuring June's code against today's session
//! binary instead.
//!
//! Two callers, one instrument:
//!   - the per-session RSS gate: a short window, assert the cap and the
//!     linearity
//!   - T4 of the public bench: a 60-second window, report the idle CPU
//!     that is the whole reason this product exists
//!
//! Spawned the way L2 spawns them -- a shm region and a control socket
//! per session -- and each one is waited for until it publishes a frame,
//! so the sample is of a settled session and not of one still starting.
//!
//!     cargo run -p marspot-session --example l3_idle_cost -- \
//!       target/release/marspot-session 9 [idle_secs]
//!
//! Prints one line: `N  total_rss_kib  per_rss_kib  cpu_secs  cpu_pct_of_core`

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{GridShmReader, create_region};

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("[idle-cost] {msg}");
    std::process::exit(1);
}

fn clear_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
}

/// `ps -o rss=,cputime=` for one pid: KiB and seconds.
fn sample(pid: u32) -> Option<(u64, f64)> {
    let out = Command::new("ps")
        .args(["-o", "rss=,cputime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rss: u64 = it.next()?.parse().ok()?;
    let mut secs = 0.0f64;
    for part in it.next()?.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some((rss, secs))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let session_bin: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_idle_cost <marspot-session> <n> [idle_secs]"));
    let n: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| die("second argument must be the session count"));
    let idle = Duration::from_secs_f64(
        args.next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(2.0),
    );
    if n == 0 {
        die("zero sessions measures nothing");
    }

    // Held so the children keep their ends; dropping these early would
    // tear down the sessions mid-measurement.
    let mut keep: Vec<(UnixStream, GridShmReader)> = Vec::with_capacity(n);
    let mut kids: Vec<std::process::Child> = Vec::with_capacity(n);

    for i in 0..n {
        let region =
            create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region #{i}: {e}")));
        clear_cloexec(region.as_raw_fd());
        let (parent, child) =
            UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair #{i}: {e}")));
        clear_cloexec(child.as_raw_fd());
        let kid = Command::new(&session_bin)
            .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
            .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
            .spawn()
            .unwrap_or_else(|e| die(format!("spawn #{i} {}: {e}", session_bin.display())));
        drop(child);
        let reader = GridShmReader::from_fd(region.as_raw_fd())
            .unwrap_or_else(|e| die(format!("reader #{i}: {e}")));
        kids.push(kid);
        keep.push((parent, reader));
    }

    // Settled means it published a frame. Measuring before that is
    // measuring a process still allocating its grid.
    let deadline = Instant::now() + Duration::from_secs(10);
    for (i, (_, reader)) in keep.iter().enumerate() {
        while reader.seq() == 0 {
            if Instant::now() > deadline {
                cleanup(&mut kids);
                die(format!("session #{i} never published a frame"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Shells print a prompt shortly after the first publish; let that
    // land so it is not charged to the idle window.
    std::thread::sleep(Duration::from_millis(600));

    let pids: Vec<u32> = kids.iter().map(|k| k.id()).collect();
    let before: Vec<(u64, f64)> = pids
        .iter()
        .map(|p| {
            sample(*p).unwrap_or_else(|| {
                die(format!("could not read pid {p} -- it is not running any more"))
            })
        })
        .collect();

    let t0 = Instant::now();
    std::thread::sleep(idle);
    let elapsed = t0.elapsed().as_secs_f64();

    let mut total_rss = 0u64;
    let mut total_cpu = 0.0f64;
    for (i, p) in pids.iter().enumerate() {
        let Some((rss, cpu)) = sample(*p) else {
            cleanup(&mut kids);
            die(format!("session #{i} (pid {p}) died during the idle window"));
        };
        total_rss += rss;
        let d = cpu - before[i].1;
        if d < 0.0 {
            cleanup(&mut kids);
            die(format!("pid {p} reported less CPU than before -- pid reuse"));
        }
        total_cpu += d;
    }

    cleanup(&mut kids);

    let per = total_rss as f64 / n as f64;
    let pct = 100.0 * total_cpu / elapsed;
    println!("{n} {total_rss} {per:.0} {total_cpu:.2} {pct:.3}");
}

/// Every child killed and reaped. A session that outlives this probe is
/// a leaked pane process, and 176 of those once came from a test that
/// did not do this.
fn cleanup(kids: &mut Vec<std::process::Child>) {
    for k in kids.iter_mut() {
        let _ = k.kill();
        let _ = k.wait();
    }
    kids.clear();
}
