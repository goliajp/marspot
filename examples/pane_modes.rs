//! One line per live session: the modes L2 reads out of the shm frame.
use std::os::fd::AsRawFd;
fn main() {
    let dir = format!("{}/Library/Caches/marspot/sessions", std::env::var("HOME").unwrap());
    let mut ids: Vec<u64> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
    ids.sort();
    for id in ids {
        let name = marspot_term::grid_shm::session_shm_name(id);
        let Ok(fd) = marspot_term::grid_shm::open_region(&name) else { continue };
        let Ok(r) = marspot_term::grid_shm::GridShmReader::from_fd(fd.as_raw_fd()) else { continue };
        let (mut c, mut w) = (Vec::new(), Vec::new());
        let Some(s) = r.read(&mut c, &mut w) else { continue };
        println!("session {id:>4}  {}x{}  alt={} alt_scroll={} sb_len={} view_off={}",
                 s.cols, s.rows, s.alt_screen(), s.alt_scroll(), s.scrollback_len, s.view_offset);
    }
}
