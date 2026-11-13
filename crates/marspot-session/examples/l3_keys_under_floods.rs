//! A key typed into one pane while other panes flood, until what the
//! pane's program did about it is in a published frame.
//!
//! `l3_flood_key_probe` types into the only pane there is.  Panes that
//! flood side by side take turns at reading, and a key can arrive while
//! its pane's reader is queued for one; this probe puts the typed pane
//! among N others that write as fast as they are read.
//!
//! The typed pane's program switches bracketed paste on or off for
//! every key it reads -- the mode travels in the frame header, so the
//! answer is seen wherever the flood has scrolled the screen to -- and
//! either floods between two keys or sleeps (`idle`).  It floods a
//! kilobyte at a time (`flooding`), which the parser keeps up with, or
//! writes a corpus 64 KiB at a time the way `cat` would
//! (`catting=<corpus>`), which with most corpora it does not.
//!
//!     l3_keys_under_floods <marspot-session> <neighbours> flooding|idle|catting=<corpus> [keys] [p99-limit-ms]
//!
//! With a limit, a p99 above it or any key left unanswered fails.
//!
//! What typing costs the flood it lands in is the other thing measured
//! here: one pane writes a corpus through while a key is typed every
//! `gap` milliseconds (0 for none), timed from the key that starts it to
//! the mode its program sets when the last byte is written.
//!
//!     l3_keys_under_floods <marspot-session> typing <corpus> <repeats> <gap-ms>

#[path = "support/mod.rs"]
mod support;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{Frame, MsgType, encode_inject_input};
use support::{Session, cleanup, die};

const COLS: u16 = 122;
const ROWS: u16 = 39;

/// Keys come this far apart and up to `GAP_SPREAD_MS` more: past the
/// moment a key keeps a reader from gathering, so every key finds its
/// pane back in the queue, at a different point of it each time.
const GAP_MS: u64 = 120;
const GAP_SPREAD_MS: u64 = 60;

const LINE: &[u8] = b"the quick brown fox jumps over the lazy dog, again and again and again and again and again and again and again\r\n";

/// The tty this program was started on, raw; `min` is how many bytes a
/// read waits for.
fn raw_tty(min: u8) {
    // SAFETY: termios calls on this process's own stdin.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(0, &mut t);
        libc::cfmakeraw(&mut t);
        t.c_cc[libc::VMIN] = min;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &t);
    }
}

fn typed_keys(buf: &mut [u8]) -> usize {
    // SAFETY: a read into a live buffer of this length.
    unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) }.max(0) as usize
}

/// What runs inside a pane.
fn pane(role: &str, rest: &[String]) -> ! {
    let mut out = std::io::stdout().lock();
    let block = |bytes: usize| LINE.iter().copied().cycle().take(bytes).collect::<Vec<u8>>();
    let mut keys = [0u8; 64];
    let mut on = false;
    let mut answer = |out: &mut std::io::StdoutLock, n: usize| {
        for _ in 0..n {
            on = !on;
            let _ = out.write_all(if on { b"\x1b[?2004h" } else { b"\x1b[?2004l" });
        }
    };
    match role {
        "flood" => {
            let block = block(64 * 1024);
            while out.write_all(&block).is_ok() {}
        }
        // one hand-over of the pty at a time, so a key is seen within
        // a kilobyte of being typed
        "flooding" => {
            raw_tty(0);
            let block = block(1024);
            while out.write_all(&block).is_ok() {
                let n = typed_keys(&mut keys);
                answer(&mut out, n);
            }
        }
        "catting" => {
            raw_tty(0);
            let corpus = std::fs::read(&rest[0]).unwrap_or_default();
            'flood: loop {
                for piece in corpus.chunks(64 * 1024) {
                    if out.write_all(piece).is_err() {
                        break 'flood;
                    }
                    let n = typed_keys(&mut keys);
                    answer(&mut out, n);
                }
            }
        }
        "idle" => {
            raw_tty(1);
            loop {
                let n = typed_keys(&mut keys);
                if n == 0 {
                    break;
                }
                answer(&mut out, n);
                let _ = out.flush();
            }
        }
        "typing" => {
            raw_tty(1);
            let corpus = std::fs::read(&rest[0]).unwrap_or_default();
            let repeats: usize = rest[1].parse().unwrap_or(1);
            typed_keys(&mut keys[..1]);
            for _ in 0..repeats {
                for piece in corpus.chunks(64 * 1024) {
                    let _ = out.write_all(piece);
                }
            }
            let _ = out.write_all(b"\x1b[?2004h");
            let _ = out.flush();
            std::thread::sleep(Duration::from_secs(600));
        }
        _ => {}
    }
    std::process::exit(0)
}

fn paste_mode(s: &Session) -> Option<bool> {
    s.reader.read(&mut Vec::new(), &mut Vec::new()).map(|f| f.bracketed_paste())
}

fn load1() -> f64 {
    let mut avg = [0f64; 1];
    // SAFETY: room for the one average asked for.
    unsafe { libc::getloadavg(avg.as_mut_ptr(), 1) };
    avg[0]
}

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("l3-keys-under-floods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs")).expect("mkdir sandbox/logs");
        std::fs::create_dir_all(dir.join("sessions")).expect("mkdir sandbox/sessions");
        // SAFETY: single-threaded here; the sessions inherit it.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        Self(dir)
    }

    /// A session whose pane runs this probe as `role`.
    fn session(&self, bin: &Path, id: u64, role: &str, rest: &str) -> Session {
        let me = std::env::current_exe().expect("this probe's own path");
        let script = self.0.join(format!("pane-{id}.sh"));
        std::fs::write(&script, format!("#!/bin/sh\nexec '{}' --pane {role} {rest}\n", me.display())).expect("write the pane script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        // SAFETY: nothing else reads the environment while panes start.
        unsafe { std::env::set_var("SHELL", &script) };
        Session::spawn_with_id(bin, COLS, ROWS, Some(id))
    }
}

/// A core reads what its session sends.
fn drain(s: &Session) {
    let mut pokes = s.control.try_clone().unwrap_or_else(|e| die(format!("clone the control stream: {e}")));
    std::thread::spawn(move || while let Ok(Some(_)) = Frame::read_from(&mut pokes) {});
}

fn keys_under_floods(bin: &Path, neighbours: usize, typed_role: &str, keys: usize, limit_ms: Option<f64>) {
    let sandbox = Sandbox::new();
    let (role, corpus) = typed_role.split_once('=').unwrap_or((typed_role, ""));
    let typed = sandbox.session(bin, 1, role, &format!("'{corpus}'"));
    let mut others: Vec<Session> = (0..neighbours).map(|i| sandbox.session(bin, i as u64 + 2, "flood", "")).collect();
    drain(&typed);
    others.iter().for_each(drain);
    typed.wait_ready("the typed pane");
    for s in &others {
        s.wait_ready("a flooding pane");
        let seq = s.reader.seq();
        s.wait_seq_past(seq, "a neighbour's flood");
    }
    // long enough for every reader to be taking turns
    std::thread::sleep(Duration::from_secs(1));

    let mut want = !paste_mode(&typed).unwrap_or(false);
    let mut samples: Vec<f64> = Vec::with_capacity(keys);
    let mut lost = 0usize;
    let (mut load_low, mut load_high) = (f64::MAX, 0f64);
    let mut spread = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..keys {
        spread = spread.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        std::thread::sleep(Duration::from_millis(GAP_MS + (spread >> 33) % GAP_SPREAD_MS));
        let load = load1();
        (load_low, load_high) = (load_low.min(load), load_high.max(load));
        let mut seen = typed.reader.seq();
        let t0 = Instant::now();
        typed.send_frame(MsgType::InjectInput, encode_inject_input(0, b"k"));
        loop {
            let seq = typed.reader.seq();
            if seq != seen {
                seen = seq;
                if paste_mode(&typed) == Some(want) {
                    samples.push(t0.elapsed().as_secs_f64() * 1000.0);
                    want = !want;
                    break;
                }
            }
            if t0.elapsed() >= Duration::from_secs(5) {
                lost += 1;
                want = !paste_mode(&typed).unwrap_or(!want);
                break;
            }
            std::hint::spin_loop();
        }
    }
    others.push(typed);
    cleanup(&mut others);
    drop(sandbox);

    if samples.is_empty() {
        die("no key was answered");
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "key→answer, {role} pane beside {neighbours} flooding, {} keys ({lost} lost): p50 {:.2} ms, p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms; load1 {load_low:.1}–{load_high:.1}",
        samples.len(),
        pick(0.50),
        pick(0.90),
        pick(0.99),
        samples[samples.len() - 1],
    );
    if let Some(limit) = limit_ms
        && (pick(0.99) > limit || lost > 0)
    {
        die(format!("p99 {:.2} ms against a limit of {limit} ms, {lost} keys unanswered", pick(0.99)));
    }
}

fn typing_cost(bin: &Path, corpus: &Path, repeats: usize, gap_ms: u64) {
    let bytes = std::fs::metadata(corpus).unwrap_or_else(|e| die(format!("{}: {e}", corpus.display()))).len() as usize * repeats;
    let sandbox = Sandbox::new();
    let session = sandbox.session(bin, 1, "typing", &format!("'{}' {repeats}", corpus.display()));
    drain(&session);
    session.wait_ready("the pane");
    let load_before = load1();
    let type_key = || session.send_frame(MsgType::InjectInput, encode_inject_input(0, b"k"));
    let started = Instant::now();
    type_key();
    let mut typed = 0u64;
    let mut seen = session.reader.seq();
    let secs = loop {
        let seq = session.reader.seq();
        if seq != seen {
            seen = seq;
            if paste_mode(&session) == Some(true) {
                break started.elapsed().as_secs_f64();
            }
        }
        if gap_ms > 0 && started.elapsed().as_millis() as u64 >= (typed + 1) * gap_ms {
            type_key();
            typed += 1;
        }
        if started.elapsed() >= Duration::from_secs(120) {
            die("the flood never ended");
        }
        std::thread::sleep(Duration::from_micros(100));
    };
    let frames = (session.reader.seq() / 2) as f64;
    let mut all = vec![session];
    cleanup(&mut all);
    drop(sandbox);
    println!(
        "{:.1} MB/s with a key every {gap_ms} ms ({typed} keys, {:.3} s, {:.0} frames/s); load1 {load_before:.1}–{:.1}",
        bytes as f64 / 1e6 / secs,
        secs,
        frames / secs,
        load1(),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--pane") {
        pane(args.get(2).map_or("", |r| r.as_str()), &args[3.min(args.len())..]);
    }
    let usage = "usage: l3_keys_under_floods <marspot-session> <neighbours> flooding|idle|catting=<corpus> [keys] [p99-limit-ms]\n       l3_keys_under_floods <marspot-session> typing <corpus> <repeats> <gap-ms>";
    if args.len() < 4 {
        die(usage);
    }
    let bin = PathBuf::from(&args[1]);
    if args[2] == "typing" {
        if args.len() < 6 {
            die(usage);
        }
        let corpus = std::fs::canonicalize(&args[3]).unwrap_or_else(|e| die(format!("{}: {e}", args[3])));
        typing_cost(&bin, &corpus, args[4].parse().unwrap_or(1), args[5].parse().unwrap_or(0));
        return;
    }
    let neighbours: usize = args[2].parse().unwrap_or_else(|_| die(usage));
    if !matches!(args[3].as_str(), "flooding" | "idle") && !args[3].starts_with("catting=/") {
        die(usage);
    }
    let keys = args.get(4).and_then(|k| k.parse().ok()).unwrap_or(100);
    let limit = args.get(5).and_then(|l| l.parse().ok());
    keys_under_floods(&bin, neighbours, &args[3], keys, limit);
}
