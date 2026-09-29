//! Does the synchronized-output gate ever see a consistent screen?
//!
//! codex keeps DEC 2026 open across almost the whole stream, closing
//! and re-opening it within a few bytes.  The gate resets only when a
//! publish happens to land while the mode is OFF, and a publish sees
//! the state AFTER the whole PTY chunk was fed — so the brief gaps are
//! invisible unless a read ends inside one.  This replays the real
//! bytelog at several chunk sizes and reports how many publishes went
//! out mid-update.
use marspot_term::terminal::Terminal;
use std::time::{Duration, Instant};

const MAX_HOLD: Duration = Duration::from_millis(150);

fn main() {
    let path = std::env::args().nth(1).expect("usage: sync_gate_probe <bytelog>");
    let bytes = std::fs::read(&path).expect("read");
    println!("{} bytes\n", bytes.len());
    println!(
        "{:>9}  {:>8}  {:>8}  {:>8}  {:>7}",
        "chunk", "publish", "torn", "clean", "torn%"
    );
    for chunk in [256usize, 1024, 4096, 16384, 65536] {
        let mut t = Terminal::new(73, 63);
        // The gate, reproduced exactly.
        let mut since: Option<Instant> = None;
        let (mut torn, mut clean) = (0u64, 0u64);
        // Wall time is not what is being measured here: the question is
        // whether the gate's `since` ever clears, so drive it with a
        // clock that advances one tick per chunk, well past the cap.
        let mut now = Instant::now();
        // The fix under test: cut each batch where the program said
        // its screen was coherent, exactly as `Session::pump` does.
        let mut carry: Vec<u8> = Vec::new();
        for c in bytes.chunks(chunk) {
            let mut buf = std::mem::take(&mut carry);
            buf.extend_from_slice(c);
            let split = if std::env::var_os("PROBE_NO_FIX").is_some() {
                buf.len()
            } else if t.uses_sync_output() {
                const ESU: &[u8] = b"\x1b[?2026l";
                match buf.windows(ESU.len()).rposition(|w| w == ESU) {
                    Some(i) => i + ESU.len(),
                    None => 0,
                }
            } else {
                buf.len()
            };
            t.feed(&buf[..split]);
            carry.extend_from_slice(&buf[split..]);
            if split == 0 {
                continue;
            }
            now += Duration::from_millis(20);
            let active = t.sync_output_active();
            let held = if !active {
                since = None;
                false
            } else {
                match since {
                    None => {
                        since = Some(now);
                        true
                    }
                    Some(s) => now.duration_since(s) < MAX_HOLD,
                }
            };
            if held {
                continue;
            }
            if active {
                torn += 1;
            } else {
                clean += 1;
            }
        }
        let total = torn + clean;
        println!(
            "{chunk:>9}  {total:>8}  {torn:>8}  {clean:>8}  {:>6.1}%",
            if total == 0 { 0.0 } else { 100.0 * torn as f64 / total as f64 }
        );
    }
}
