//! Feed a file to a terminal and report what the feed cost.
//!
//!     cargo run --release -p marspot-term --example parse_cost -- FILE [REPEAT [CHUNK [raw]]]
//!
//! The engine alone: no pty, no reader thread, no window.  It builds on
//! every target the engine builds on, so the same run can sit under
//! `perf stat` / `perf record` on Linux and under `/usr/bin/time -l` on
//! macOS.  With `MARSPOT_SESSION_ID` and `MARSPOT_STATE_DIR` set the
//! terminal files its history the way a pane does; without them the
//! history stays in memory.
//!
//! `CHUNK` is how many bytes each `feed` call gets (default 65536, the
//! reader's block; 0 = the whole file in one call).  Only `feed` is
//! timed; each repeat starts from a fresh terminal.
//!
//! A pty turns each line feed a program writes into CR LF before the
//! terminal reads it, and so does this, ahead of the clock; `raw` feeds
//! the file as it is.  The per-byte figures divide by the file's size
//! either way, which is what a throughput over the same file divides by.

use marspot_term::terminal::Terminal;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: parse_cost FILE [REPEAT [CHUNK [raw]]]");
        std::process::exit(2);
    };
    let repeat: u32 = args.next().map_or(1, |s| s.parse().expect("REPEAT is a number"));
    let chunk: usize = args.next().map_or(65536, |s| s.parse().expect("CHUNK is a number"));
    let raw = args.next().is_some_and(|s| s == "raw");
    let file = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2);
    });
    let mut bytes = Vec::with_capacity(file.len() + file.len() / 16);
    for &b in &file {
        if b == b'\n' && !raw {
            bytes.push(b'\r');
        }
        bytes.push(b);
    }
    let mut ns: u128 = 0;
    for _ in 0..repeat {
        let mut terminal = Terminal::new(122, 39);
        let t0 = Instant::now();
        if chunk == 0 {
            terminal.feed(&bytes);
        } else {
            for part in bytes.chunks(chunk) {
                terminal.feed(part);
            }
        }
        ns += t0.elapsed().as_nanos();
    }
    let total = file.len() as u128 * repeat as u128;
    println!(
        r#"{{"path":"{path}","bytes":{},"repeat":{repeat},"chunk":{chunk},"ns_per_byte":{:.4},"mb_per_s":{:.1}}}"#,
        file.len(),
        ns as f64 / total as f64,
        total as f64 * 1e3 / ns as f64,
    );
}
