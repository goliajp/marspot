//! How much of a real stream is a grapheme cluster of more than one
//! codepoint?
//!
//! `S2-02` proposes putting multi-codepoint clusters in a side pool
//! and storing an index in the cell.  Whether that pool needs
//! reference counting or can be swept with its row depends on how many
//! cells point into it, and that is a question about real output, not
//! about Unicode.
//!
//!   cargo run -p marspot-term --example cluster_census -- <file>…
//!
//! Each file is read as raw terminal output: escape sequences are
//! stripped the crude way (they are not text and would otherwise count
//! as ASCII clusters), and what is left is segmented.

use marspot_term::grapheme::graphemes;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: cluster_census <file>…");
        std::process::exit(2);
    }
    println!(
        "{:<34} {:>10} {:>10} {:>8}  widest",
        "stream", "clusters", "multi", "share"
    );
    for path in &args {
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("{path}: unreadable");
            continue;
        };
        let text = strip_escapes(&bytes);
        let mut total = 0usize;
        let mut multi = 0usize;
        let mut widest = 0usize;
        let mut example = String::new();
        for g in graphemes(&text) {
            // `\r\n` is one cluster under UAX #29 and would otherwise be
            // the commonest "multi-codepoint" thing in any terminal
            // stream by an order of magnitude.  The terminal never
            // prints it as a cluster — CR and LF are control codes
            // handled before the segmenter sees text — so counting it
            // here measures the file's line endings, not its text.
            let first = g.chars().next().unwrap_or(' ');
            if (first as u32) < 0x20 || first == '\u{7f}' {
                continue;
            }
            total += 1;
            let n = g.chars().count();
            if n > 1 {
                multi += 1;
                if n > widest {
                    widest = n;
                    example = g.to_string();
                }
            }
        }
        let share = if total == 0 { 0.0 } else { multi as f64 * 100.0 / total as f64 };
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        println!(
            "{name:<34} {total:>10} {multi:>10} {share:>7.3}%  {widest} cp {example:?}"
        );
    }
}

/// Drop CSI / OSC / simple escape sequences, then decode what is left.
///
/// Crude on purpose: this is a census, and the alternative is driving
/// a whole `Terminal` to get a number that only needs the text.  The
/// one thing it must not do is let escape bytes through, because they
/// are ASCII and would dilute the very ratio being measured.
fn strip_escapes(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            Some(b'[') => {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                i += 1;
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                if bytes.get(i) == Some(&0x1b) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Some(_) => i += 2,
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
