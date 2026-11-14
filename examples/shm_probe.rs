//! Look at, or take away, the shared regions of one session from
//! outside the app.
//!
//!   shm_probe title  <session dir>    print the published window title
//!   shm_probe screen <session dir>    print the published frame, cluster text included
//!   shm_probe unlink <session dir>    remove the grid region's name
//!
//! `unlink` leaves the session and its mappings alone: it is what
//! another program removing the name looks like to the app.

fn main() {
    let what = std::env::args().nth(1).expect("title | unlink");
    let dir = std::path::PathBuf::from(std::env::args().nth(2).expect("session dir"));
    if let Some(root) = dir.parent().and_then(|p| p.parent()) {
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
    let toml = std::fs::read_to_string(dir.join("entry.toml")).expect("entry.toml");
    let entry = marspot_term::session_registry::parse_session_entry_text(&toml).expect("entry");
    let regions = marspot_term::grid_shm::SessionRegions::of_entry(&entry).expect("shm_name");
    match what.as_str() {
        "title" => {
            let title = marspot_term::grid_shm::title::read(&regions).expect("title region");
            println!("{}", title.unwrap_or_default());
        }
        "screen" => {
            // the published frame and nothing else: no fall back to
            // the session's record
            let f = marspot_term::grid_shm::peek_frame(&regions).expect("grid region").expect("a frame");
            for row in 0..f.rows {
                let mut line = String::new();
                for col in 0..f.cols {
                    match f.clusters.iter().find(|c| (c.0, c.1) == (col, row)) {
                        Some(c) => line.push_str(&c.2),
                        None => line.push(f.cells[row as usize * f.cols as usize + col as usize].ch),
                    }
                }
                println!("{}", line.replace('\0', "").trim_end());
            }
        }
        "unlink" => {
            marspot_term::grid_shm::delete_region(regions.grid_name());
            println!("unlinked {}", entry.shm_name);
        }
        other => panic!("unknown command {other}"),
    }
}
