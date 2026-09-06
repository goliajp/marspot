//! Type into a pane the way a keyboard does — through the KEY path.
//!
//! `inject_keys` writes to the PTY directly, which is what a program
//! sends, not what a person types: it never touches local-echo
//! prediction.  Prediction lives on the key path, so measuring it
//! needs a probe that goes the same way a keystroke does.
//!
//!   cargo run --release --example type_keys -- <session-dir> <text>
use std::io::Write;
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers};
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("usage: type_keys <session-dir> <text>");
    let text = a.next().expect("text to type");
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
    for ch in text.chars() {
        let ev = MarspotKeyEvent {
            logical: LogicalKey::Char(ch),
            state: KeyState::Pressed,
            text: Some(ch.to_string()),
        };
        let wire = event_to_wire(&ev, Modifiers::default());
        Frame::new(MsgType::KeyEvent, encode_key_event(&wire, 0))
            .write_to(&mut ctl)
            .expect("send");
        ctl.flush().ok();
        // Human cadence: predictions are made per keystroke and
        // reconciled against what comes back, so typing faster than a
        // person would measures something else.
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    println!("typed {} chars into session {sid}", text.chars().count());
}
