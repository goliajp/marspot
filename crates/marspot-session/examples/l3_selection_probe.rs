//! The text under a selection comes from L3, not from L2's mirror.
//!
//! Cmd-C has to answer with what is actually on screen, and L2 cannot:
//! its mirror is window-only, so a selection that reaches into history has
//! nothing behind it there. The real grid lives in the session, so the
//! copy is a request to it -- `GetSelectionText` out, `SelectionText`
//! back, carrying a request id so a late reply from a timed-out request
//! cannot be mistaken for the next copy.
//!
//! This types a marker, selects the row it landed on, and asserts the
//! reply contains it. A broken round trip returns empty or stale text,
//! both of which look like "copy did nothing" to a user.
//!
//! Lost with the June history rebuild; rewritten 2026-10-01.
//!
//!     cargo run -p marspot-session --example l3_selection_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use marspot_term::shell_proto::{MsgType, decode_selection_text, encode_get_selection_text};
use std::path::PathBuf;
use std::time::Duration;
use support::{COLS, ROWS, Session, cleanup, die};

const MARKER: &str = "selmark";
const REQ_SEQ: u32 = 4242;

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_selection_probe <marspot-session>"));

    let session = Session::spawn(&bin, COLS, ROWS);
    session.wait_ready("session");

    // Put the marker on a line of its own via the shell, so it is output
    // rather than a half-typed command line.
    session.send_line(&format!("printf '%s\\n' {MARKER}"));
    // Wait for it to appear rather than sleeping a guess: the shell has to
    // fork, run, and have its output parsed and published.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut found_row: Option<u16> = None;
    while found_row.is_none() {
        for row in 0..ROWS {
            let line: String = (0..COLS).map(|c| session.cell(c, row, COLS)).collect();
            // The echoed command line also contains the marker, so take
            // the LAST row that has it on its own -- that is the output.
            if line.trim_end() == MARKER {
                found_row = Some(row);
            }
        }
        if found_row.is_none() && std::time::Instant::now() >= deadline {
            let mut all = vec![session];
            cleanup(&mut all);
            die(format!("{MARKER:?} never appeared on a line of its own"));
        }
    }
    let row = found_row.unwrap();

    // `abs` in this request is a view offset counted back from the live
    // bottom, not a forward line number -- `grid_selection_text` reads it
    // as one ("bigger abs = older") and passes it straight to
    // `cell_at_view` with the last view row.  So the top screen row has
    // the LARGEST abs.  Passing the screen row directly selected a line
    // near the bottom of the screen and returned "".
    let abs = (ROWS - 1 - row) as u32;
    session.send_frame(
        MsgType::GetSelectionText,
        encode_get_selection_text(REQ_SEQ, (0, abs), (MARKER.len() as u16, abs), false),
    );
    let reply = session.await_frame(MsgType::SelectionText, 10);
    let (seq, text) = decode_selection_text(&reply.payload)
        .unwrap_or_else(|e| die(format!("decoding SelectionText: {e}")));

    let mut all = vec![session];
    cleanup(&mut all);

    if seq != REQ_SEQ {
        die(format!("asked with seq {REQ_SEQ} and got {seq} -- a reply this copy did not make"));
    }
    if !text.contains(MARKER) {
        die(format!(
            "selected the row holding {MARKER:?} and got {text:?} back"
        ));
    }
    println!("PASS: GetSelectionText(seq {seq}) returned {text:?} from L3's own grid");
}
