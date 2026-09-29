//! Count the distinct screens a pane publishes over a window.
//!
//! The user-visible cost of ignoring synchronized output is not a
//! number in a log — it is how many half-drawn screens reach the
//! display during one repaint.  So count them: poll the published
//! snapshot fast and report how many DIFFERENT ones went by.
//!
//!   cargo run --example frame_count -- <session-dir> <millis>
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("session dir");
    let ms: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(1500);
    if let Some(root) = std::path::Path::new(&dir).parent().and_then(|p| p.parent()) {
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", root) };
    }
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    let (mut last, mut distinct, mut polls) = (0u64, 0u64, 0u64);
    // The symptom is "it goes black", so measure blackness: the
    // emptiest screen that reached the display, and how many were
    // mostly empty.
    let (mut min_fill, mut near_blank) = (1.0f32, 0u64);
    let mut fills: Vec<f32> = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Some(_s) = r.read(&mut cells, &mut wrapped) {
            let mut h = DefaultHasher::new();
            for cell in &cells {
                cell.ch.hash(&mut h);
            }
            let d = h.finish();
            if d != last {
                distinct += 1;
                last = d;
                let used = cells
                    .iter()
                    .filter(|c| c.ch != ' ' && c.ch != '\0')
                    .count();
                let fill = used as f32 / cells.len().max(1) as f32;
                fills.push(fill);
                if fill < min_fill {
                    min_fill = fill;
                }
                if fill < 0.10 {
                    near_blank += 1;
                }
            }
        }
        polls += 1;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    println!("{distinct} distinct screens over {ms}ms ({polls} polls)");
    println!("  emptiest screen shown: {:.1}% filled", min_fill * 100.0);
    println!("  screens under 10% filled: {near_blank}");
    let series: Vec<String> = fills.iter().map(|f| format!("{:.0}%", f * 100.0)).collect();
    println!("  fill sequence: {}", series.join(" -> "));
}
