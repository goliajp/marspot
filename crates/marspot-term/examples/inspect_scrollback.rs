//! inspect_scrollback — replay FileScrollback::open against an
//! on-disk session's files and report exactly what happens.
//!
//! Usage: `cargo run --release --example inspect_scrollback -- <bin> <idx> <cols> <ram_capacity>`
//!
//! Prints:
//!   - total_lines after open()
//!   - hot vs cold split
//!   - ram_capacity & load_n
//!   - for the last `ram_capacity` entries: for each, success / Err string
//!   - whether the open() itself errored
//!
//! Run against `~/Library/Caches/marspot/sessions/<sid>/scrollback.{bin,idx}`
//! for any session you want to diagnose.

use marspot_term::scrollback::FileScrollback;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        eprintln!("usage: inspect_scrollback <bin> <idx> <cols> <ram_capacity>");
        std::process::exit(2);
    }
    let bin_path = std::path::PathBuf::from(&args[0]);
    let idx_path = std::path::PathBuf::from(&args[1]);
    let cols: usize = args[2].parse().expect("cols");
    let ram_capacity: usize = args[3].parse().expect("ram_capacity");

    println!("=== inspect: {} ===", bin_path.display());
    let bin_size = std::fs::metadata(&bin_path).map(|m| m.len()).unwrap_or(0);
    let idx_size = std::fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);
    println!("  bin size = {bin_size}");
    println!("  idx size = {idx_size}   ({} entries)", idx_size / 8);

    match FileScrollback::open(bin_path, idx_path, cols, ram_capacity) {
        Ok(sb) => {
            println!("  open()  = Ok");
            println!("  len()   = {}", sb.len());
            println!("  capacity= {}", sb.capacity());
            // Try to read line 0 and last line.
            let n = sb.len();
            if n > 0 {
                // Sample the last 20 lines — what user sees first when scrolling up.
                let mut blank_count = 0usize;
                let sample_n = n.min(20);
                let sample_first = n - sample_n;
                for li in sample_first..n {
                    if let Some(line) = sb.read_line(li) {
                        let is_blank = line.iter().all(|c| c.ch == ' ' || c.ch == '\0');
                        if is_blank {
                            blank_count += 1;
                        }
                    }
                }
                println!("  recent {sample_n}-line scroll-up blanks = {blank_count}/{sample_n}");
                // Print the first 5 visible non-blank chars
                for li in [
                    sample_first,
                    n.saturating_sub(50),
                    n.saturating_sub(200),
                    n.saturating_sub(1000),
                    n.saturating_sub(5000),
                ] {
                    if let Some(line) = sb.read_line(li) {
                        let txt: String = line
                            .iter()
                            .take(60)
                            .map(|c| if c.ch == '\0' { '·' } else { c.ch })
                            .collect();
                        println!("  line[{li}] = {:?}", txt.trim_end());
                    }
                }
            }
        }
        Err(e) => {
            println!("  open() FAILED: {e}");
        }
    }
}
