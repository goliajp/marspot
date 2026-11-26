//! The Claude record fold started at a line boundary partway into a
//! file says the same `used` rows as `allocation_usage::Readers::read`,
//! which starts at the end of a file the first time it sees it.

use std::fmt::Write as _;
use std::path::Path;

use marspot_adapters::claude::fold;
use marspot_agent::types::RecordFold;

use super::super::allocation_prices::Tokens as RowTokens;
use super::super::allocation_usage::Readers;
use super::fold_check_tests::{Sink, cut_plans, records, rows_of_fold};

/// The rows `Readers` says of what is appended after it first saw the
/// file holding `bytes[..start]`.
fn rows_of_readers_from(path: &Path, bytes: &[u8], start: usize) -> String {
    std::fs::write(path, &bytes[..start]).unwrap();
    let mut r = Readers::default();
    assert!(r.read(path, 0).is_empty());
    std::fs::write(path, bytes).unwrap();
    let mut total = RowTokens::default();
    let mut s = String::new();
    for sp in r.read(path, 0) {
        total.add(&sp.tokens);
        writeln!(s, "{} {} {}", sp.at, sp.model, sp.tokens.words()).unwrap();
    }
    s + &format!("total {}\n", total.words())
}

fn folded_from(bytes: &[u8], start: usize, cuts: &[usize]) -> String {
    let (mut st, mut sink) = (RecordFold::from_boundary(start as u64), Sink::default());
    for cut in cuts.iter().map(|c| c + start).chain([bytes.len()]) {
        let from = st.at as usize;
        if cut > from && cut <= bytes.len() {
            fold(&bytes[from..cut], &mut st, &mut sink);
        }
    }
    assert_eq!(st.at as usize, bytes.len(), "every line was folded");
    rows_of_fold(&st, &sink)
}

#[test]
fn claudecode_fold_from_a_line_boundary_says_what_readers_say_from_the_end() {
    let tmp = std::env::temp_dir().join(format!("marspot-fold-tail-check-{}.jsonl", std::process::id()));
    let mut appended_rows = 0;
    for path in records() {
        let bytes = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let starts: Vec<usize> = std::iter::once(0).chain(bytes.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1)).collect();
        let middle = starts[starts.len() / 2];
        for &start in &starts {
            let rows = rows_of_readers_from(&tmp, &bytes, start);
            appended_rows += rows.lines().count() - 1;
            let plans = if start == middle { cut_plans(bytes.len() - start) } else { vec![Vec::new()] };
            for cuts in plans {
                assert_eq!(folded_from(&bytes, start, &cuts), rows, "{name}: from byte {start}, {} cuts", cuts.len());
            }
        }
    }
    assert!(appended_rows > 0, "some start had replies after it");
    std::fs::remove_file(&tmp).unwrap();
}
