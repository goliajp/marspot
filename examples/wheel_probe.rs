//! Ask, on a LIVE pane, what one wheel tick does to the picture.
//!
//!   cargo run --release --example wheel_probe -- <session-id> [ticks]
//!
//! Injects `ticks` wheel-up SGR reports, reads the published frame,
//! then injects the same number of wheel-down and reads again — so the
//! pane ends where it started.  Reports how well each new frame aligns
//! to the one before it as a pure vertical shift, which is exactly what
//! re-anchoring a selection to an app-driven scroll would need.
use marspot_term::shell_proto::{encode_inject_input, Frame, MsgType};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;

use std::os::fd::AsRawFd;

/// The visible picture as text, built the way `Pane::screen_rows`
/// builds it so the tape sees exactly what it would see live.
fn frame_text(id: u64) -> Option<Vec<String>> {
    let name = marspot_term::grid_shm::session_shm_name(id);
    let fd = marspot_term::grid_shm::open_region(&name).ok()?;
    let r = marspot_term::grid_shm::GridShmReader::from_fd(fd.as_raw_fd()).ok()?;
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped)?;
    let (cols, rows) = (s.cols as usize, s.rows as usize);
    Some(
        (0..rows)
            .map(|r0| {
                let mut line = String::new();
                for c in 0..cols {
                    let ch = cells[r0 * cols + c].ch;
                    if ch != '\0' {
                        line.push(ch);
                    }
                }
                line.truncate(line.trim_end().len());
                line
            })
            .collect(),
    )
}

fn frame_rows(id: u64) -> Option<(Vec<u64>, usize, u16)> {
    let name = marspot_term::grid_shm::session_shm_name(id);
    let fd = marspot_term::grid_shm::open_region(&name).ok()?;
    let r = marspot_term::grid_shm::GridShmReader::from_fd(fd.as_raw_fd()).ok()?;
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped)?;
    let (cols, rows) = (s.cols as usize, s.rows as usize);
    let mut hs = Vec::with_capacity(rows);
    let mut live = 0;
    for r0 in 0..rows {
        let mut h = DefaultHasher::new();
        let mut blank = true;
        for c in 0..cols {
            let ch = cells[r0 * cols + c].ch;
            let ch = if ch == '\0' { ' ' } else { ch };
            if ch != ' ' { blank = false; }
            ch.hash(&mut h);
        }
        if !blank { live += 1; }
        hs.push(h.finish());
    }
    Some((hs, live, s.rows))
}

fn best_shift(old: &[u64], new: &[u64]) -> (i32, usize) {
    let n = old.len() as i32;
    let mut best = (0i32, 0usize);
    for s in -(n - 1)..n {
        let mut m = 0;
        for r in 0..n {
            let src = r + s;
            if src < 0 || src >= n { continue; }
            if new[r as usize] == old[src as usize] { m += 1; }
        }
        if m > best.1 { best = (s, m); }
    }
    best
}

fn main() {
    let id: u64 = std::env::args().nth(1).unwrap().parse().unwrap();
    let ticks: u32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(3);
    let mut sock = marspot_term::uds_session_client::wait_and_connect(
        id, std::time::Duration::from_secs(3),
    ).expect("connect to the session");
    let send = |sock: &mut std::os::unix::net::UnixStream, up: bool, n: u32| {
        let b = if up { 64 } else { 65 };
        let mut buf = Vec::new();
        for _ in 0..n {
            buf.extend_from_slice(format!("\x1b[<{b};10;10M").as_bytes());
        }
        let f = Frame::new(MsgType::InjectInput, encode_inject_input(id, &buf));
        f.write_to(sock).unwrap();
        sock.flush().unwrap();
    };
    let (f0, live0, rows) = frame_rows(id).expect("frame");
    // Fold the same frames through the real tape: what the shift
    // detector says is only useful if the tape keeps a line's text
    // under the same virtual number across it.
    let t0 = frame_text(id).expect("text");
    let anchor_row = (rows / 2) as u16;
    let anchored_text = t0[anchor_row as usize].clone();
    let mut tape = marspot::selection_tape::SelectionTape::new(0, &t0, 0, anchor_row);
    send(&mut sock, true, ticks);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (f1, live1, _) = frame_rows(id).expect("frame");
    let fold_up = tape.fold(&frame_text(id).expect("text"));
    send(&mut sock, false, ticks);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (f2, live2, _) = frame_rows(id).expect("frame");
    let fold_down = tape.fold(&frame_text(id).expect("text"));
    let landed = frame_text(id).expect("text");
    let anchor_screen_row = tape.abs(tape.anchor.1, rows);
    let anchor_now = (rows as i64 - 1 - anchor_screen_row as i64) as usize;
    let still_on_it = landed.get(anchor_now) == Some(&anchored_text);

    // Leave the pane where it was found: the down ticks may not undo
    // the up ones exactly (the program clamps at the ends of its own
    // transcript), so nudge one tick at a time until the picture is
    // the one we started from.
    let mut nudges = 0;
    let mut f2 = f2;
    while f2 != f0 && nudges < 8 {
        let (s, _) = best_shift(&f0, &f2);
        if s == 0 { break; }
        send(&mut sock, s > 0, 1);
        std::thread::sleep(std::time::Duration::from_millis(300));
        f2 = frame_rows(id).expect("frame").0;
        nudges += 1;
    }
    let (s1, m1) = best_shift(&f0, &f1);
    let (s2, m2) = best_shift(&f1, &f2);
    println!("session {id}, {rows} rows, {ticks} ticks");
    println!("  up:   shift {s1:>3}, {m1}/{rows} rows align (live {live0} → {live1})");
    println!("  down: shift {s2:>3}, {m2}/{rows} rows align (live {live1} → {live2})");
    println!("  back where it started: {} (after {nudges} corrective ticks)", f0 == f2);
    println!("  tape read the up as {fold_up:?}, the down as {fold_down:?}, broken={}", tape.broken);
    println!("  anchor still names the text it was put on: {still_on_it}");
}
