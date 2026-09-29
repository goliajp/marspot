//! Measure how long a keystroke takes to come back as PTY echo.
//!
//! The local-echo prediction deadline has to sit above the slowest
//! legitimate echo and below "a human notices a stray character".
//! This probe supplies the lower half of that sandwich: it types one
//! byte at a time into a real shell and times the round trip, both on
//! an idle prompt and while the shell is busy producing output.
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use std::io::ErrorKind;
use std::time::{Duration, Instant};

fn drain(pty: &mut Pty, quiet_for: Duration, cap: Duration) {
    let start = Instant::now();
    let mut last = Instant::now();
    let mut buf = [0u8; 8192];
    while start.elapsed() < cap {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => last = Instant::now(),
            _ => {
                if last.elapsed() >= quiet_for {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Type `byte`, then read until it shows up.  Returns the round trip.
fn rtt(pty: &mut Pty, byte: u8) -> Option<Duration> {
    let t0 = Instant::now();
    pty.write(&[byte]).ok()?;
    let mut buf = [0u8; 8192];
    while t0.elapsed() < Duration::from_secs(2) {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => {
                if buf[..n].contains(&byte) {
                    return Some(t0.elapsed());
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            _ => {}
        }
    }
    None
}

fn report(label: &str, mut v: Vec<Duration>) {
    if v.is_empty() {
        println!("{label:<22} (no samples)");
        return;
    }
    v.sort();
    let us = |d: &Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{label:<22} n={:<3} min {:.2}ms  p50 {:.2}ms  p95 {:.2}ms  max {:.2}ms",
        v.len(),
        us(&v[0]),
        us(&v[v.len() / 2]),
        us(&v[v.len() * 95 / 100]),
        us(&v[v.len() - 1]),
    );
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    // --- idle zsh prompt -------------------------------------------
    let mut pty = Pty::spawn(PtyConfig {
        program: "/bin/zsh".into(),
        argv0: Some("-zsh".into()),
        size: TerminalSize { cols: 120, rows: 40, ..Default::default() },
        env_remove_prefixes: vec!["MARSPOT_".into()],
        ..Default::default()
    })
    .expect("spawn zsh");
    // The master fd is blocking by default; this probe polls it.
    unsafe {
        let fd = pty.raw_master();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    drain(&mut pty, Duration::from_millis(400), Duration::from_secs(6));

    let mut idle = Vec::new();
    for i in 0..n {
        let b = b'a' + (i % 26) as u8;
        if let Some(d) = rtt(&mut pty, b) {
            idle.push(d);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    report("idle zsh prompt", idle);

    // Clear the line so the busy command lands clean.
    let _ = pty.write(&[0x15]); // Ctrl-U
    drain(&mut pty, Duration::from_millis(200), Duration::from_secs(2));

    // --- shell busy printing ---------------------------------------
    // A background loop keeps the PTY saturated while we type at the
    // prompt: the echo now has to share the pipe with real output.
    let _ = pty.write(b"(while true; do print -n 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done) &\r");
    std::thread::sleep(Duration::from_millis(700));
    let mut busy = Vec::new();
    for i in 0..n {
        let b = b'a' + (i % 26) as u8;
        if let Some(d) = rtt(&mut pty, b) {
            busy.push(d);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    report("zsh + flooding bg", busy);
}
