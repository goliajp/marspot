//! Census of every escape sequence a real program actually sends.
//!
//! "Which sequences does codex use" is answerable from its own bytes
//! rather than from its source or from guesswork, and the answer is
//! what says whether a gap is real.  Scans a bytelog and tallies CSI
//! finals (with their private-mode parameters), OSC numbers, and ESC
//! dispatches by count.
use std::collections::BTreeMap;

fn main() {
    let path = std::env::args().nth(1).expect("usage: escape_census <bytelog>");
    let d = std::fs::read(&path).expect("read");
    let mut csi: BTreeMap<String, u64> = BTreeMap::new();
    let mut osc: BTreeMap<String, u64> = BTreeMap::new();
    let mut esc: BTreeMap<String, u64> = BTreeMap::new();
    let mut i = 0;
    while i < d.len() {
        if d[i] != 0x1b {
            i += 1;
            continue;
        }
        let Some(&next) = d.get(i + 1) else { break };
        match next {
            b'[' => {
                let mut j = i + 2;
                let start = j;
                while j < d.len() && (0x20..=0x3f).contains(&d[j]) {
                    j += 1;
                }
                if j >= d.len() {
                    break;
                }
                let params = String::from_utf8_lossy(&d[start..j]).to_string();
                let fin = d[j] as char;
                // Group private modes by their number; everything else
                // by final byte alone, so the tally stays readable.
                let key = if params.starts_with('?') {
                    format!("CSI ?{params} {fin}", params = &params[1..])
                } else {
                    format!("CSI {fin}")
                };
                *csi.entry(key).or_default() += 1;
                i = j + 1;
            }
            b']' => {
                let mut j = i + 2;
                let start = j;
                while j < d.len() && d[j].is_ascii_digit() {
                    j += 1;
                }
                let num = String::from_utf8_lossy(&d[start..j]).to_string();
                *osc.entry(format!("OSC {num}")).or_default() += 1;
                while j < d.len() && d[j] != 0x07 && d[j] != 0x1b {
                    j += 1;
                }
                i = j + 1;
            }
            _ => {
                *esc.entry(format!("ESC {}", next as char)).or_default() += 1;
                i += 2;
            }
        }
    }
    let mut show = |title: &str, m: &BTreeMap<String, u64>| {
        println!("\n{title}");
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in v {
            println!("  {n:>9}  {k}");
        }
    };
    show("CSI", &csi);
    show("OSC", &osc);
    show("ESC", &esc);
}
