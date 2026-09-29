//! Print every line a scrollback pair holds, cell by cell, in a form
//! that does not depend on how `Cell` or `Color` are represented in
//! this build: the codepoint in hex, then the 13-byte attribute
//! encoding (`serialize_attrs_pub`, which every build has had).
//!
//! Its job is comparing two builds on the same history — the output of
//! the build before a storage-format change and the build after it
//! must be byte-identical.  Point it at COPIES: opening a pair lets
//! the writer reconcile the index, and a newer build may move an older
//! header forward.
//!
//!   cargo run -p marspot-term --release --example scrollback_dump -- <bin> <idx> <cols>
use marspot_term::scrollback::FileScrollback;
use std::io::Write;

fn main() {
    let mut a = std::env::args().skip(1);
    let bin = std::path::PathBuf::from(a.next().expect("bin"));
    let idx = std::path::PathBuf::from(a.next().expect("idx"));
    let cols: usize = a.next().expect("cols").parse().expect("cols is a number");
    let sb = FileScrollback::open(bin, idx, cols, 4).expect("open");
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    let n = sb.len();
    for i in 0..n {
        let line = sb.read_line(i).expect("every counted line reads");
        write!(out, "{i}:").unwrap();
        for c in &line {
            write!(out, " {:x}/", c.ch as u32).unwrap();
            for b in marspot_term::terminal::serialize_attrs_pub(c.attrs) {
                write!(out, "{b:02x}").unwrap();
            }
        }
        writeln!(out).unwrap();
    }
    eprintln!("{n} lines");
}
