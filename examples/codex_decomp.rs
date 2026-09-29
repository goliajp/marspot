//! Where does a codex pane's time actually go?
//!
//! Replays a real bytelog through the same Terminal + shm publish the
//! session process uses, and reports the per-stage cost.  The
//! synthetic `cat-ascii` bench says 400 MB/s; a TUI stream is nothing
//! like `cat`, and the only way to know what it costs is to run it.
use marspot_term::terminal::Terminal;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: codex_decomp <bytelog>");
    let bytes = std::fs::read(&path).expect("read");
    let (cols, rows) = (73u16, 63u16);
    let mb = bytes.len() as f64 / 1e6;
    println!("{:.1} MB, {cols}x{rows}\n", mb);

    // --- S1  parse + grid, whole stream in one feed ----------------
    let mut t = Terminal::new(cols, rows);
    let t0 = Instant::now();
    t.feed(&bytes);
    let one_shot = t0.elapsed();
    println!(
        "S1 parse+grid (one feed)      {:>8.1} ms   {:>7.1} MB/s",
        one_shot.as_secs_f64() * 1e3,
        mb / one_shot.as_secs_f64()
    );

    // --- S2  the same, cut at update boundaries like pump does ------
    const ESU: &[u8] = b"\x1b[?2026l";
    let mut t = Terminal::new(cols, rows);
    let mut carry: Vec<u8> = Vec::new();
    let mut publishes = 0u64;
    let mut fed_total = 0usize;
    let t0 = Instant::now();
    for c in bytes.chunks(4096) {
        let mut buf = std::mem::take(&mut carry);
        buf.extend_from_slice(c);
        let split = if t.uses_sync_output() {
            match buf.windows(ESU.len()).rposition(|w| w == ESU) {
                Some(i) => i + ESU.len(),
                None => 0,
            }
        } else {
            buf.len()
        };
        t.feed(&buf[..split]);
        fed_total += split;
        carry.extend_from_slice(&buf[split..]);
        if split > 0 {
            publishes += 1;
        }
    }
    let chunked = t0.elapsed();
    println!(
        "S2 + boundary scan (4 KiB)    {:>8.1} ms   {:>7.1} MB/s   (+{:.1}% over S1)",
        chunked.as_secs_f64() * 1e3,
        mb / chunked.as_secs_f64(),
        100.0 * (chunked.as_secs_f64() / one_shot.as_secs_f64() - 1.0)
    );
    println!("   fed {} B in {publishes} publishes", fed_total);

    // --- S3  how much of the grid actually changes per publish ------
    let cells = cols as usize * rows as usize;
    let mut t = Terminal::new(cols, rows);
    let mut prev: Vec<char> = vec!['\0'; cells];
    let mut carry: Vec<u8> = Vec::new();
    let (mut sum_changed, mut n, mut identical) = (0u64, 0u64, 0u64);
    let mut snap = vec!['\0'; cells];
    let t0 = Instant::now();
    for c in bytes.chunks(4096) {
        let mut buf = std::mem::take(&mut carry);
        buf.extend_from_slice(c);
        let split = if t.uses_sync_output() {
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
        let g = t.grid();
        for r in 0..rows {
            for cc in 0..cols {
                snap[r as usize * cols as usize + cc as usize] = g.cell(cc, r).ch;
            }
        }
        let changed = snap.iter().zip(prev.iter()).filter(|(a, b)| a != b).count();
        if changed == 0 {
            identical += 1;
        }
        sum_changed += changed as u64;
        n += 1;
        prev.copy_from_slice(&snap);
    }
    let scan = t0.elapsed();
    println!(
        "\nS3 per published frame        {:>8.1} cells changed of {cells}  ({:.2}%)",
        sum_changed as f64 / n.max(1) as f64,
        100.0 * sum_changed as f64 / (n.max(1) * cells as u64) as f64
    );
    println!(
        "   {n} frames, {identical} byte-identical ({:.1}% — those never wake L2)",
        100.0 * identical as f64 / n.max(1) as f64
    );
    println!("   (snapshot+diff overhead in this probe: {:.0} ms)", scan.as_secs_f64() * 1e3);
}
