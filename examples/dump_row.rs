//! Print the first N rows of a session's grid, with repr-style escaping.
use std::ffi::CString;
fn main() {
    let dir = std::env::args().nth(1).expect("session dir");
    let n: usize = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(3);
    if let Some(root) = std::path::Path::new(&dir).parent().and_then(|p| p.parent()) {
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
    let toml = std::fs::read_to_string(format!("{dir}/entry.toml")).unwrap();
    let name = toml.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "shm_name").then(|| v.trim().trim_matches('"').to_string())
    }).unwrap();
    let c = CString::new(name).unwrap();
    let fd = marspot_term::grid_shm::open_region(&c).unwrap();
    let r = marspot_term::grid_shm::GridShmReader::from_fd(
        std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped).expect("snapshot");
    for row in 0..n.min(s.rows as usize) {
        let line: String = (0..s.cols as usize)
            .map(|c| cells[row * s.cols as usize + c].ch)
            .collect();
        // Take by chars, not bytes: a CJK row would panic on a byte
        // slice exactly when the dump is needed most.
        let head = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        println!("  row{row} raw      {:?}", head(&line, 48));
        let squeezed: String = line.chars().filter(|c| !c.is_whitespace() && *c != '\0').collect();
        println!("  row{row} squeezed {:?}", head(&squeezed, 32));
    }
}
