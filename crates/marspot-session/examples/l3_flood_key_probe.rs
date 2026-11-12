//! A key typed into a pane that is flooding, until what the program
//! did about it is in a published frame.
//!
//! The pane's program floods it and, for every key it finds typed,
//! switches bracketed paste on or off.  That mode travels in the frame
//! header, so the probe sees the answer wherever the flood has scrolled
//! the screen to; one program writes both, so the answer is never cut
//! in two by the flood.  One sample covers the control socket, the loop taking
//! the key between two slices of parsing, the write to the PTY, the
//! program, its answer queued behind the flood, the parse, and the
//! publish.
//!
//! Keys come `GAP_MS` apart, long enough that nothing about one key is
//! still in force for the next.
//!
//!     cargo run --release -p marspot-session --example l3_flood_key_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{Frame, MsgType, encode_inject_input};
use support::{COLS, ROWS, Session, cleanup, die};

const KEYS: usize = 60;
const GAP_MS: u64 = 300;

fn paste_mode(s: &Session) -> Option<bool> {
    s.reader.read(&mut Vec::new(), &mut Vec::new()).map(|f| f.bracketed_paste())
}

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_flood_key_probe <marspot-session>"));
    let sandbox = std::env::temp_dir().join(format!("l3-flood-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    // gone however this ends: a failure here is a panic, which unwinds
    struct Sandbox(PathBuf);
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _sandbox = Sandbox(sandbox.clone());
    std::fs::create_dir_all(sandbox.join("logs")).expect("mkdir sandbox/logs");
    std::fs::create_dir_all(sandbox.join("sessions")).expect("mkdir sandbox/sessions");
    let script = sandbox.join("flood.sh");
    std::fs::write(
        &script,
        "#!/bin/zsh\nstty -icanon -echo min 1 time 0\nblock=''\nrepeat 60 block+=$'the quick brown fox jumps over the lazy dog\\r\\n'\non=0\nwhile :; do\n  print -rn -- $block\n  if read -t 0 -k1 k; then\n    if (( on )); then print -n '\\e[?2004l'; on=0; else print -n '\\e[?2004h'; on=1; fi\n  fi\ndone\n",
    )
    .expect("write the flood script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // SAFETY: single-threaded here; the session inherits both.
    unsafe {
        std::env::set_var("MARSPOT_STATE_DIR", &sandbox);
        std::env::set_var("SHELL", &script);
    }

    let session = Session::spawn(&bin, COLS, ROWS);
    // a core reads what its session sends
    let mut pokes = session.control.try_clone().unwrap_or_else(|e| die(format!("clone the control stream: {e}")));
    std::thread::spawn(move || while let Ok(Some(_)) = Frame::read_from(&mut pokes) {});
    session.wait_ready("session");
    // flooding: frames keep coming without anything being typed
    let seq = session.reader.seq();
    session.wait_seq_past(seq, "the flood");

    let mut want = !paste_mode(&session).unwrap_or(false);
    let mut samples: Vec<f64> = Vec::with_capacity(KEYS);
    let mut lost = 0usize;
    let frames_from = session.reader.seq();
    let started = Instant::now();
    for _ in 0..KEYS {
        std::thread::sleep(Duration::from_millis(GAP_MS));
        let t0 = Instant::now();
        session.send_frame(MsgType::InjectInput, encode_inject_input(0, b"k"));
        loop {
            if paste_mode(&session) == Some(want) {
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
                want = !want;
                break;
            }
            if t0.elapsed() >= Duration::from_secs(5) {
                // the answer was split by the flood, or never came; the
                // next key's answer may arrive either way round
                lost += 1;
                want = !paste_mode(&session).unwrap_or(!want);
                break;
            }
            std::thread::yield_now();
        }
    }
    let frames = (session.reader.seq() - frames_from) / 2;
    let secs = started.elapsed().as_secs_f64();
    let mut all = vec![session];
    cleanup(&mut all);

    if samples.len() < KEYS / 2 {
        die(format!("only {} of {KEYS} keys were answered", samples.len()));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "key→answer in a published frame, pane flooding, {} keys ({lost} lost): p50 {:.2} ms, p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms; {:.0} frames/s",
        samples.len(),
        pick(0.50),
        pick(0.90),
        pick(0.99),
        samples[samples.len() - 1],
        frames as f64 / secs,
    );
}
