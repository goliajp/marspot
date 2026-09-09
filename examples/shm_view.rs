//! Watch what L2 actually sees for a live session, straight out of the
//! shared framebuffer.
//!
//!   cargo run --release --example shm_view -- <session-id>          # one dump
//!   cargo run --release --example shm_view -- <session-id> --watch  # catch a
//!                                                                   # blank band
//!
//! Written for the "scroll a codex pane and a black band appears"
//! report, where every offline model of the pane is clean — the only
//! thing left to look at is the frame the live L3 publishes while the
//! user is scrolling.
use std::os::fd::AsRawFd;

fn main() {
    let id: u64 = std::env::args().nth(1).expect("session id").parse().expect("number");
    let watch = std::env::args().any(|a| a == "--watch");
    let name = marspot_term::grid_shm::session_shm_name(id);
    let fd = marspot_term::grid_shm::open_region(&name).expect("open shm");
    let r = marspot_term::grid_shm::GridShmReader::from_fd(fd.as_raw_fd()).expect("map");
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let mut last_seq = 0u64;
    let mut caught = 0usize;
    loop {
        let Some(snap) = r.read(&mut cells, &mut wrapped) else { continue };
        let (cols, rows) = (snap.cols as usize, snap.rows as usize);
        let mut run = 0usize;
        let mut best = (0usize, 0usize);
        let mut top_blank = 0usize;
        let mut seen_content = false;
        let mut lines: Vec<String> = Vec::with_capacity(rows);
        for r0 in 0..rows {
            let line: String = (0..cols)
                .map(|c| { let ch = cells[r0 * cols + c].ch; if ch == '\0' { ' ' } else { ch } })
                .collect();
            if line.trim().is_empty() {
                run += 1;
                if run > best.1 { best = (r0 + 1 - run, run); }
                if !seen_content { top_blank = run; }
            } else { run = 0; seen_content = true; }
            lines.push(line.trim_end().to_string());
        }
        // The reported shape: a black band ABOVE the content.  The
        // startup / resume picker is the opposite (content on top,
        // blank below) and would otherwise burn the catch budget.
        let interesting = !watch || (top_blank >= 20 && seen_content);
        if interesting && r.seq() != last_seq {
            last_seq = r.seq();
            caught += 1;
            println!(
                "--- session {id} {cols}x{rows} view_offset={} scrollback_len={} \
                 top_blank={} longest_blank_run={} at r{}",
                snap.view_offset, snap.scrollback_len, top_blank, best.1, best.0
            );
            for (i, l) in lines.iter().enumerate() { println!("r{i:>2}|{l}|"); }
            if !watch || caught >= 6 { return; }
        }
        if !watch { return; }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}
