//! Print the live mode flags a session is publishing.
use std::ffi::CString;
fn main() {
    let dir = std::env::args().nth(1).expect("usage: pane_flags <session-dir>");
    let toml = std::fs::read_to_string(format!("{dir}/entry.toml")).expect("entry.toml");
    let name = toml.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "shm_name").then(|| v.trim().trim_matches('"').to_string())
    }).expect("shm_name");
    let c = CString::new(name.clone()).unwrap();
    let fd = marspot_term::grid_shm::open_region(&c).expect("open shm");
    let r = marspot_term::grid_shm::GridShmReader::from_fd(
        std::os::fd::AsRawFd::as_raw_fd(&fd)).expect("reader");
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped).expect("no snapshot published");
    use marspot_term::grid_shm as g;
    println!("shm {name}  {}x{}", s.cols, s.rows);
    println!("  flags = 0x{:x}   scrollback_len = {}   view_offset = {}   scroll_push = {}",
             s.flags, s.scrollback_len, s.view_offset, s.scroll_push_count);
    for (bit, label) in [
        (g::FLAG_MOUSE_TRACKING, "MOUSE_TRACKING"),
        (g::FLAG_MOUSE_SGR, "MOUSE_SGR"),
        (g::FLAG_CURSOR_VISIBLE, "CURSOR_VISIBLE"),
        (g::FLAG_BRACKETED_PASTE, "BRACKETED_PASTE"),
        (g::FLAG_APP_CURSOR_KEYS, "APP_CURSOR_KEYS"),
    ] {
        println!("    {:<16} {}", label, if s.flags & bit != 0 { "on" } else { "off" });
    }
}
