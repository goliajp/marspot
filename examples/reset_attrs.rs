//! Put a pane's pen back to plain.
//!
//! A style belongs to the program that set it, and normally the
//! program — or the shell's next prompt — turns it off.  A full-screen
//! program can run for hours emitting no `CSI 0 m` at all, so a style
//! switched on by anything ELSE rides every new cell until that
//! program exits.  Telling the user to quit their session is not a
//! fix.
//!
//!   cargo run --release --example reset_attrs -- <session-dir>
use std::io::Write;
use marspot_term::shell_proto::{encode_pane_reset_attrs, Frame, MsgType};

fn main() {
    let dir = std::env::args().nth(1).expect("usage: reset_attrs <session-dir>");
    // The registry is keyed off MARSPOT_STATE_DIR; without it the
    // session looks like it was never registered.
    if let Some(root) = std::path::Path::new(&dir).parent().and_then(|p| p.parent()) {
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
    let sid: u64 = std::path::Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse().ok())
        .expect("session dir is named by its id");
    let mut ctl = marspot_term::uds_session_client::wait_and_connect(
        sid,
        std::time::Duration::from_secs(10),
    )
    .expect("connect");
    Frame::new(MsgType::PaneResetAttrs, encode_pane_reset_attrs(sid))
        .write_to(&mut ctl)
        .expect("send");
    ctl.flush().ok();
    println!("session {sid}: pen reset");
}
