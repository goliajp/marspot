use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

/// Kills and reaps the child it holds, whatever the test's outcome.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn until<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() > end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_child_left_unreaped_reads_as_a_zombie_of_its_parent() {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id() as i32;
    let me = std::process::id() as i32;
    let z = until(Duration::from_secs(5), || zombies().into_iter().find(|z| z.pid == pid));
    let z = z.expect("the exited, unreaped child is listed");
    assert_eq!(z.ppid, me);
    assert!(z.start_us > 0, "the start time is read: {z:?}");
    assert!(crate::pidtree::proc_id(pid).is_none(), "proc_pidinfo answers nothing for a zombie");
    child.wait().unwrap();
    assert!(!zombies().iter().any(|z| z.pid == pid), "reaped, it is gone from the list");
}

#[test]
fn zombies_lists_no_live_process() {
    let child = Reaped(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let pid = child.0.id() as i32;
    let all = zombies();
    assert!(!all.iter().any(|z| z.pid == pid || z.pid == std::process::id() as i32));
}

#[test]
fn fd_path_names_the_file_a_process_writes_to() {
    let dir = std::env::temp_dir().join(format!("fdpath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("b1234.output");
    let out = std::fs::File::create(&file).unwrap();
    let child = Reaped(Command::new("/bin/sleep").arg("30").stdout(out).stdin(Stdio::null()).spawn().unwrap());
    let pid = child.0.id() as i32;
    assert_eq!(fd_path(pid, 1), Some(file.canonicalize().unwrap()));
    assert_eq!(fd_path(pid, 0).as_deref(), Some(std::path::Path::new("/dev/null")));
    assert_eq!(fd_path(pid, 57), None, "no such descriptor");
    drop(child);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fd_path_of_a_pipe_is_none() {
    let child = Reaped(Command::new("/bin/sleep").arg("30").stdout(Stdio::piped()).spawn().unwrap());
    assert_eq!(fd_path(child.0.id() as i32, 1), None);
}

#[test]
fn cpu_with_children_is_in_nanoseconds() {
    let me = std::process::id() as i32;
    let before = cpu_with_children(me).unwrap();
    let t = Instant::now();
    let mut x = 0u64;
    while t.elapsed() < Duration::from_millis(300) {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    let spent = cpu_with_children(me).unwrap() - before;
    // one busy thread for 300 ms of wall time: at most that, at least a tenth on a loaded host
    assert!((30_000_000..=1_000_000_000).contains(&spent), "spent {spent} ns");
}

#[test]
fn proc_cpu_time_is_in_nanoseconds() {
    let me = std::process::id() as i32;
    let before = proc_cpu_time_ns(me).unwrap();
    let t = Instant::now();
    let mut x = 0u64;
    while t.elapsed() < Duration::from_millis(300) {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    let spent = proc_cpu_time_ns(me).unwrap() - before;
    assert!((30_000_000..=1_000_000_000).contains(&spent), "spent {spent} ns");
}

#[test]
fn cpu_with_children_grows_for_a_script_that_starts_a_child_every_second() {
    let script = "while :; do /bin/sh -c 'i=0; while [ $i -lt 3000 ]; do i=$((i+1)); done'; sleep 1; done";
    let child = Reaped(Command::new("/bin/sh").arg("-c").arg(script).spawn().unwrap());
    let pid = child.0.id() as i32;
    std::thread::sleep(Duration::from_millis(500));
    let first = cpu_with_children(pid).unwrap();
    let own_first = proc_cpu_time_ns(pid).unwrap();
    let mut readings = vec![first];
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(1100));
        readings.push(cpu_with_children(pid).unwrap());
    }
    eprintln!("cpu_with_children over 3.3 s: {readings:?}");
    assert!(readings.windows(2).all(|w| w[1] >= w[0]), "never falls: {readings:?}");
    let grew = readings[3] - first;
    assert!(grew > 3 * 1_000_000, "the reaped children's CPU is counted: grew {grew} ns");
    let own_grew = proc_cpu_time_ns(pid).unwrap() - own_first;
    assert!(grew > own_grew * 2, "most of it is the children's: {grew} vs own {own_grew}");
}

#[test]
fn cpu_with_children_of_no_process_is_none() {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id() as i32;
    child.wait().unwrap();
    assert_eq!(cpu_with_children(pid), None);
}

#[test]
fn proc_stat_cpu_of_a_spinning_child_tracks_wall_time() {
    let child = Reaped(Command::new("/bin/sh").arg("-c").arg("while :; do :; done").spawn().unwrap());
    let pid = child.0.id() as i32;
    let first = crate::pidtree::proc_stat(pid).unwrap();
    let t = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    let second = crate::pidtree::proc_stat(pid).unwrap();
    let wall = t.elapsed().as_nanos() as u64;
    let spent = second.total_cpu_ns - first.total_cpu_ns;
    // one spinning thread: about its wall time, less only on a loaded host
    assert!(spent * 10 >= wall * 3 && spent * 10 <= wall * 12, "spent {spent} ns in {wall} ns");
    let pct = second.cpu_pct_since(first.total_cpu_ns, wall);
    assert!((30.0..=120.0).contains(&pct), "panel CPU% {pct}");
    assert_eq!(second.cpu_pct_since(first.total_cpu_ns, 0), 0.0);
    assert_eq!(first.cpu_pct_since(second.total_cpu_ns, wall), 0.0);
}
