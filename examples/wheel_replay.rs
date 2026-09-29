//! Replay L2's wheel loop against a live session and report each tick.
//!
//! This is the end-to-end check for the "scrolling flickers and drops
//! out" class of bug.  It runs `marspot::wheel_marker::shows_marker` —
//! the same predicate L2 decides with, not a copy — over the session's
//! real published grid, then sends the plugin's real key bytes into
//! the real program, and prints what the screen did.
//!
//! A healthy run enters once and then stays open, with the content
//! moving.  `open` flapping back to false is the toggle being re-sent.
//!
//!   cargo run --example wheel_replay -- <session-dir> <sid> \
//!       <marker> <enter> <up> [ticks]
//!
//! Byte strings accept `\xNN` and `\e`.
use std::io::Write;
use marspot_term::shell_proto::{encode_inject_input, Frame, MsgType};

fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = s.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'e') => out.push(0x1b),
            Some(b'x') => {
                let hex: String =
                    (0..2).filter_map(|_| it.next()).map(|c| c as char).collect();
                out.push(u8::from_str_radix(&hex, 16).expect("\\xNN"));
            }
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    out
}

/// The cells the session is publishing right now, as (cols, rows, chars).
fn snapshot(dir: &str) -> (u16, u16, Vec<char>, bool) {
    let toml = std::fs::read_to_string(format!("{dir}/entry.toml")).unwrap();
    let name = toml
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "shm_name").then(|| v.trim().trim_matches('"').to_string())
        })
        .expect("shm_name");
    let c = std::ffi::CString::new(name).unwrap();
    let fd = marspot_term::grid_shm::open_region(&c).expect("open shm");
    let r = marspot_term::grid_shm::GridShmReader::from_fd(
        std::os::fd::AsRawFd::as_raw_fd(&fd),
    )
    .expect("reader");
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped).expect("no snapshot published");
    (s.cols, s.rows, cells.iter().map(|c| c.ch).collect(), s.alt_scroll())
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 5 {
        eprintln!(
            "usage: wheel_replay <session-dir> <sid> <marker> <enter> <key> [ticks]\n\
             set WHEEL_DOWN=1 to replay a downward turn"
        );
        std::process::exit(2);
    }
    let (dir, sid) = (a[0].clone(), a[1].parse::<u64>().expect("sid"));
    let (marker, enter, key) = (unescape(&a[2]), unescape(&a[3]), unescape(&a[4]));
    let ticks: usize = a.get(5).map(|s| s.parse().unwrap()).unwrap_or(6);
    // Which way the wheel is being turned; a closed view is only ours
    // when that is up.
    let up = std::env::var("WHEEL_DOWN").is_err();

    // The registry is keyed off MARSPOT_STATE_DIR; without it the
    // sandbox session looks like it was never registered.
    if let Some(root) = std::path::Path::new(&dir).parent().and_then(|p| p.parent()) {
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
    let mut ctl = marspot_term::uds_session_client::wait_and_connect(
        sid,
        std::time::Duration::from_secs(10),
    )
    .expect("connect");
    let mut send = |bytes: &[u8]| {
        Frame::new(MsgType::InjectInput, encode_inject_input(sid, bytes))
            .write_to(&mut ctl)
            .expect("send");
        ctl.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(700));
    };

    let mut entered = 0usize;
    let mut prev: Option<Vec<char>> = None;
    for tick in 1..=ticks {
        let (cols, rows, cells, alt_scroll) = snapshot(&dir);
        let open = marspot::wheel_marker::view_is_open(
            alt_scroll,
            cols,
            rows,
            |col, row| cells[row as usize * cols as usize + col as usize],
            &marker,
        );
        // Exactly what L2 does per wheel event.
        let ours = marspot::wheel_marker::wheel_is_ours(open, up);
        if ours {
            if !open && !enter.is_empty() {
                send(&enter);
                entered += 1;
            }
            send(&key);
        }
        let (_, _, after, _) = snapshot(&dir);
        let moved = prev.as_ref().map(|p| *p != after).unwrap_or(true);
        println!(
            "tick {tick}: open_before={open}  ours={ours}  sent_enter={}  content_moved={moved}",
            ours && !open && !enter.is_empty()
        );
        prev = Some(after);
    }

    let (cols, rows, cells, alt_scroll_end) = snapshot(&dir);
    let open_end = marspot::wheel_marker::view_is_open(
        alt_scroll_end,
        cols,
        rows,
        |col, row| cells[row as usize * cols as usize + col as usize],
        &marker,
    );
    println!("\nentered {entered}x over {ticks} ticks; open at end = {open_end}");
    if entered > 1 {
        println!("FAIL: entered {entered}x — the toggle is being re-sent");
        std::process::exit(1);
    }
    if up && !open_end {
        println!("FAIL: view is not open at the end");
        std::process::exit(1);
    }
    if !up && entered > 0 {
        println!("FAIL: a downward turn opened the view");
        std::process::exit(1);
    }
    match (entered, open_end) {
        (0, false) => println!("PASS: left the pane alone, view stayed closed"),
        (0, true) => println!("PASS: was already open, never pressed the toggle"),
        _ => println!("PASS: entered once, stayed open"),
    }
}
