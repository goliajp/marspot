//! Cost gates: the time hot or recurring work takes.  Each budget is 3x
//! the P95 measured on the reference machine (mini2, Apple silicon Mac
//! mini, debug test build) and is widened only after measuring again.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use marspot::pidtree::{self, ProcId, ProcRow, ProcTable};
use marspot::zombie::check::{PaneIn, Scene, Watch};
use marspot::zombie::files::PaneFiles;
use marspot::zombie::probe::Live;

/// One zombie-watch check over 50 claude panes with 10 tasks each and a
/// 200 MB transcript each, every task long silent so that every check
/// samples every task's CPU.  P95 2.31 to 5.80 ms over nine runs on mini2
/// 2026-11-28, load1 4.9 to 6.9 from other jobs (11.5 ms on mini 2026-11-26).
const ZOMBIE_CHECK_P95_BUDGET: Duration = Duration::from_micros(17_401);
/// One check of four panes whose transcripts each grew by [`APPENDED`]
/// since the last check, all of it read and folded, as a multiple of the
/// floor: reading the same appended bytes once and counting their lines,
/// timed in the same run.  A fixed time failed on a hosted runner about
/// 3x slower than mini; the ratio holds across machines and still blows
/// when a check re-reads whole transcripts or folds them quadratically.
/// P95/floor 1.52 to 1.85 over six runs (P95 176 to 263 ms, floor p50
/// 116 to 144 ms) measured 2026-11-26.
const ZOMBIE_APPENDED_P95_FLOOR_MULTIPLE: f64 = 5.5;
const APPENDED_PANES: usize = 4;
const APPENDED: usize = 6 << 20;
const APPENDED_CHECKS: u64 = 20;

const PANES: usize = 50;
const TASKS: usize = 10;
const TRANSCRIPT: u64 = 200 << 20;

/// The stand-in claudes and every task under them, killed by pid when
/// the test ends however it ends.
struct Procs {
    agents: Vec<Child>,
    tasks: Vec<ProcId>,
    dir: PathBuf,
}

impl Drop for Procs {
    fn drop(&mut self) {
        for t in &self.tasks {
            let _ = pidtree::signal(*t, libc::SIGKILL);
        }
        for a in &mut self.agents {
            let _ = a.kill();
            let _ = a.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn transcript(path: &Path, pane: usize) {
    let f = File::create(path).unwrap();
    // all but the tail is a hole: the first read only touches the last 64 KiB
    f.set_len(TRANSCRIPT - 64 * 1024).unwrap();
    let mut tail = String::from("\n");
    for i in 0..TASKS {
        tail.push_str(&format!(
            r#"{{"type":"assistant","timestamp":"2026-11-25T05:57:06.000Z","message":{{"stop_reason":"tool_use","content":[{{"type":"tool_use","id":"c{i}","name":"Bash","input":{{"command":"sleep 120 # {pane}","run_in_background":true}}}}]}}}}"#
        ));
        tail.push('\n');
        tail.push_str(&format!(
            r#"{{"type":"user","timestamp":"2026-11-25T05:57:07.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"c{i}","content":"Command running in background with ID: b{pane:02}{i:02}"}}]}}}}"#
        ));
        tail.push('\n');
    }
    tail.push_str(r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-11-25T05:58:00.000Z"}"#);
    tail.push('\n');
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(tail.as_bytes()).unwrap();
}

fn spawn(dir: &Path, panes: usize) -> Procs {
    let mut procs = Procs { agents: Vec::new(), tasks: Vec::new(), dir: dir.to_path_buf() };
    for p in 0..panes {
        let tasks = dir.join(format!("p{p}")).join("tasks");
        fs::create_dir_all(&tasks).unwrap();
        // job control puts each task in its own group, as claude's are
        let script = format!(
            "set -m; for i in $(seq -f %02g 0 {}); do sleep 120 > {}/b{p:02}$i.output & done; wait",
            TASKS - 1,
            tasks.display()
        );
        let child = Command::new("/bin/bash").arg("-c").arg(script).stdin(Stdio::null()).spawn().unwrap();
        procs.agents.push(child);
        transcript(&dir.join(format!("u{p}.jsonl")), p);
    }
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let t = ProcTable::take();
        let n: usize = procs.agents.iter().map(|a| t.children(a.id() as i32).count()).sum();
        if n == panes * TASKS {
            procs.tasks = procs.agents.iter().flat_map(|a| t.children(a.id() as i32).map(ProcRow::id).collect::<Vec<_>>()).collect();
            break;
        }
        assert!(Instant::now() < end, "{n} of {} tasks started", panes * TASKS);
        std::thread::sleep(Duration::from_millis(50));
    }
    // three hours unwritten: past B1's silence, so every check samples CPU
    let old = SystemTime::now() - Duration::from_secs(3 * 3600);
    for e in fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().is_dir()) {
        for f in fs::read_dir(e.path().join("tasks")).unwrap().flatten() {
            File::options().write(true).open(f.path()).unwrap().set_modified(old).unwrap();
        }
    }
    procs
}

fn uuid(p: usize) -> String {
    format!("u{p}")
}

#[test]
fn zombie_check_perf_gate() {
    let dir = std::env::temp_dir().join(format!("zombie-perf-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let procs = spawn(&dir, PANES);
    let table = ProcTable::take();
    let agents: Vec<&ProcRow> = procs.agents.iter().map(|a| table.get(a.id() as i32).unwrap()).collect();
    let mut files: Vec<PaneFiles> = agents.iter().enumerate().map(|(p, a)| PaneFiles::new(a.id(), &uuid(p), &dir)).collect();
    let mut watch = Watch::default();
    let mut probe = Live::default();
    let helper = |_: &ProcRow, _: bool| false;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    let mut times = Vec::new();
    let mut hits = 0;
    for i in 0..100u64 {
        let t0 = Instant::now();
        for (f, a) in files.iter_mut().zip(&agents) {
            f.refresh(a.start_unix);
        }
        let panes = files.iter().zip(&agents).enumerate().map(|(p, (f, a))| PaneIn { sid: p as u64, agent: a, log: &f.log }).collect();
        let scene = Scene { now: now + i * 300, table: &table, panes, outside: Vec::new(), uid: 501, tmpdir: None, helper: &helper };
        hits = watch.hits(&scene, &mut probe).len();
        times.push(t0.elapsed());
    }
    let first = times[0];
    let read: u64 = files.iter().map(PaneFiles::read_bytes).sum();
    times.sort();
    let p95 = times[94];
    println!("zombie check: first {first:?}, p50 {:?}, p95 {p95:?}, max {:?}; {hits} hits; {read} transcript bytes read; {} syscalls", times[49], times[99], probe.calls);
    assert!(read <= (PANES as u64) * 64 * 1024, "only each transcript's last 64 KiB is ever read: {read}");
    assert_eq!(hits, PANES * (TASKS - 1), "every older copy of a pane's identical command");
    // a hosted CI runner (`CI` set) is shared and of unknown speed: the
    // time is printed there and the read bytes and hits still hold
    if std::env::var_os("CI").is_none() {
        assert!(p95 < ZOMBIE_CHECK_P95_BUDGET, "p95 {p95:?} over the budget {ZOMBIE_CHECK_P95_BUDGET:?}");
    }
    drop(procs);
}

/// What a busy session adds between two checks: replies, tool calls and
/// results, the odd background call, a turn's end.  Lines of the sizes
/// claude writes, about 1.5 KiB each.
fn growth(bytes: usize) -> String {
    let filler = "x".repeat(1500);
    let mut out = String::with_capacity(bytes + 4096);
    let mut i = 0usize;
    while out.len() < bytes {
        let line = match i % 50 {
            0 => format!(
                r#"{{"type":"assistant","timestamp":"2026-11-25T06:00:00.000Z","message":{{"stop_reason":"tool_use","content":[{{"type":"tool_use","id":"g{i}","name":"Bash","input":{{"command":"sleep 1 # {i}","run_in_background":true}}}}]}}}}"#
            ),
            1 => format!(
                r#"{{"type":"user","timestamp":"2026-11-25T06:00:01.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"g{i}","content":"Command running in background with ID: g{i}"}}]}}}}"#
            ),
            49 => r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-11-25T06:00:02.000Z"}"#.to_string(),
            n if n % 2 == 0 => format!(
                r#"{{"type":"assistant","timestamp":"2026-11-25T06:00:00.000Z","message":{{"stop_reason":"end_turn","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
            ),
            _ => format!(
                r#"{{"type":"user","timestamp":"2026-11-25T06:00:01.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"t{i}","content":"{filler}"}}]}}}}"#
            ),
        };
        out.push_str(&line);
        out.push('\n');
        i += 1;
    }
    out
}

#[test]
fn zombie_check_perf_gate_with_megabytes_appended_between_checks() {
    let dir = std::env::temp_dir().join(format!("zombie-perf-grow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let procs = spawn(&dir, APPENDED_PANES);
    let table = ProcTable::take();
    let agents: Vec<&ProcRow> = procs.agents.iter().map(|a| table.get(a.id() as i32).unwrap()).collect();
    let mut files: Vec<PaneFiles> = agents.iter().enumerate().map(|(p, a)| PaneFiles::new(a.id(), &uuid(p), &dir)).collect();
    for (f, a) in files.iter_mut().zip(&agents) {
        f.refresh(a.start_unix);
    }
    let chunk = growth(APPENDED);
    let mut watch = Watch::default();
    let mut probe = Live::default();
    let helper = |_: &ProcRow, _: bool| false;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    let mut times = Vec::new();
    let mut floors = Vec::new();
    let mut marks: Vec<u64> = (0..APPENDED_PANES).map(|p| fs::metadata(dir.join(format!("u{p}.jsonl"))).unwrap().len()).collect();
    let before: u64 = files.iter().map(PaneFiles::read_bytes).sum();
    for i in 0..APPENDED_CHECKS {
        for p in 0..APPENDED_PANES {
            OpenOptions::new().append(true).open(dir.join(format!("u{p}.jsonl"))).unwrap().write_all(chunk.as_bytes()).unwrap();
        }
        let t0 = Instant::now();
        for (f, a) in files.iter_mut().zip(&agents) {
            f.refresh(a.start_unix);
        }
        let panes = files.iter().zip(&agents).enumerate().map(|(p, (f, a))| PaneIn { sid: p as u64, agent: a, log: &f.log }).collect();
        let scene = Scene { now: now + i * 300, table: &table, panes, outside: Vec::new(), uid: 501, tmpdir: None, helper: &helper };
        watch.hits(&scene, &mut probe);
        times.push(t0.elapsed());
        let t1 = Instant::now();
        std::hint::black_box(floor_read(&dir, &mut marks));
        floors.push(t1.elapsed());
    }
    let read = files.iter().map(PaneFiles::read_bytes).sum::<u64>() - before;
    times.sort();
    floors.sort();
    let p95 = times[(times.len() * 95).div_ceil(100) - 1];
    let floor = floors[floors.len() / 2];
    let ratio = p95.as_secs_f64() / floor.as_secs_f64();
    println!(
        "zombie check with {} MiB appended per pane: p50 {:?}, p95 {p95:?}, max {:?}; floor p50 {floor:?}, max {:?}; p95/floor {ratio:.2}; {read} bytes read",
        APPENDED >> 20,
        times[times.len() / 2],
        times[times.len() - 1],
        floors[floors.len() - 1]
    );
    assert_eq!(read, (chunk.len() * APPENDED_PANES) as u64 * APPENDED_CHECKS, "every appended byte is read once");
    assert!(
        ratio < ZOMBIE_APPENDED_P95_FLOOR_MULTIPLE,
        "p95 {p95:?} is {ratio:.2}x the floor {floor:?}, over the budget {ZOMBIE_APPENDED_P95_FLOOR_MULTIPLE}x"
    );
    drop(procs);
}

/// The least any check must do with what was appended: read each pane's
/// new bytes once and find the line ends.  Returns the line count.
fn floor_read(dir: &Path, marks: &mut [u64]) -> usize {
    let mut lines = 0;
    for (p, mark) in marks.iter_mut().enumerate() {
        let mut f = File::open(dir.join(format!("u{p}.jsonl"))).unwrap();
        f.seek(SeekFrom::Start(*mark)).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        *mark += buf.len() as u64;
        lines += buf.iter().filter(|b| **b == b'\n').count();
    }
    lines
}
