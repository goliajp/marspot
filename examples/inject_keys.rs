//! Inject a key sequence into a session and dump the screen before/after.
use std::io::Write;
use marspot_term::shell_proto::{encode_inject_input, Frame, MsgType};

fn dump(dir: &str) -> String {
    let toml = std::fs::read_to_string(format!("{dir}/entry.toml")).unwrap();
    let name = toml.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "shm_name").then(|| v.trim().trim_matches('"').to_string())
    }).unwrap();
    let c = std::ffi::CString::new(name).unwrap();
    let fd = marspot_term::grid_shm::open_region(&c).unwrap();
    let r = marspot_term::grid_shm::GridShmReader::from_fd(
        std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let Some(s) = r.read(&mut cells, &mut wrapped) else { return String::new() };
    let mut out = String::new();
    for row in 0..s.rows as usize {
        for col in 0..s.cols as usize {
            out.push(cells[row * s.cols as usize + col].ch);
        }
        out.push('\n');
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).expect("session dir");
    // The registry is keyed off MARSPOT_STATE_DIR; without it
    // wait_and_connect looks in the installed app's state root and
    // reports the sandbox session as never registered.
    if let Some(root) = std::path::Path::new(&dir).parent().and_then(|p| p.parent()) {
        // SAFETY: single-threaded, before anything reads it.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
    let sid: u64 = std::env::args().nth(2).expect("sid").parse().unwrap();
    let keys = std::env::args().nth(3).expect("keys: pageup|pagedown|ctrl-t|up|down");
    let typed: Vec<u8>;
    let bytes: &[u8] = match keys.as_str() {
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "ctrl-t" => b"\x14",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "enter" => b"\r",
        other => {
            //  sends the text verbatim — used to drive the
            // program into a state worth testing (a screen with more
            // history than fits) before a key is measured.
            if let Some(t) = other.strip_prefix("type:") {
                typed = t.as_bytes().to_vec();
                &typed
            } else {
                panic!("unknown key: {other}")
            }
        }
    };
    let before = dump(&dir);
    let mut ctl = marspot_term::uds_session_client::wait_and_connect(
        sid, std::time::Duration::from_secs(10)).expect("connect");
    let f = Frame::new(MsgType::InjectInput, encode_inject_input(sid, bytes));
    f.write_to(&mut ctl).expect("send");
    ctl.flush().ok();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let after = dump(&dir);
    let changed = before != after;
    println!("key={keys}  screen_changed={changed}");
    if changed {
        let b: Vec<&str> = before.lines().collect();
        let a: Vec<&str> = after.lines().collect();
        let n = b.len().min(a.len());
        let diff = (0..n).filter(|i| b[*i] != a[*i]).count();
        println!("  rows differing: {diff}/{n}");
        for i in 0..n.min(6) {
            if b[i] != a[i] {
                println!("    row{i} before |{}|", b[i].trim_end());
                println!("    row{i} after  |{}|", a[i].trim_end());
            }
        }
    }
}
