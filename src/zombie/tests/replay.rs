//! The rules over the processes of 2026-11-25 23:34 JST.
//!
//! Fields the 23:34 table lacks (process group, standard output, working
//! directory, environment) come from the same processes at 00:01, matched
//! by pid and start time.  The three watchdogs of pane 488 stopped in
//! between have their task taken from the tool result that started them
//! (same second as the process); their outputs' last write is unknown,
//! because the stop rewrote them.  A field the scene does not have is
//! absent, and a rule that needs it does not judge.  Commands, paths and
//! names are replaced by placeholders that keep what the rules compare:
//! which commands are the same, which files a tail follows, which kind of
//! place a working directory is.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use crate::pidtree::{ProcTable, Zombie};

use super::super::check::{OutsideIn, PaneIn, Scene, Watch};
use super::super::rules::{self, Rule};
use super::super::transcript::ClaudeLog;
use super::{MapProbe, fixtures, helper, row};

#[derive(Default, Clone)]
struct Env {
    session: Option<String>,
    token: Option<String>,
    thread: Option<String>,
}

struct Fixture {
    now: u64,
    table: ProcTable,
    probe: MapProbe,
    env: HashMap<i32, Env>,
    argv: HashMap<i32, Vec<String>>,
    /// (sid, shell, agent kind, session or thread)
    panes: Vec<(u64, i32, String, String)>,
}

fn field(s: &str) -> Option<String> {
    (s != "-").then(|| s.to_string())
}

fn load() -> Fixture {
    let dir = fixtures();
    let mut f = Fixture {
        now: 0,
        table: ProcTable::from_rows(Vec::new()),
        probe: MapProbe::default(),
        env: HashMap::new(),
        argv: HashMap::new(),
        panes: Vec::new(),
    };
    let mut rows = Vec::new();
    for line in fs::read_to_string(dir.join("scene.tsv")).unwrap().lines() {
        if let Some(now) = line.strip_prefix("# now\t") {
            f.now = now.parse().unwrap();
        }
        if line.starts_with('#') {
            continue;
        }
        let c: Vec<&str> = line.splitn(12, '\t').collect();
        let pid: i32 = c[0].parse().unwrap();
        rows.push(row(pid, c[1].parse().unwrap(), c[2].parse().unwrap(), c[3].parse().unwrap(), c[4]));
        let path = |s: &str| field(s).map(PathBuf::from);
        if let Some(p) = path(c[5]) {
            f.probe.cwd.insert(pid, p);
        }
        if let Some(p) = path(c[6]) {
            f.probe.exe.insert(pid, p);
        }
        if let Some(p) = path(c[7]) {
            f.probe.stdout.insert(pid, p);
        }
        f.env.insert(pid, Env { session: field(c[8]), token: field(c[9]), thread: field(c[10]) });
        let argv: Vec<String> = c[11].split(' ').map(str::to_string).collect();
        f.probe.argv.insert(pid, argv.clone());
        f.argv.insert(pid, argv);
    }
    f.table = ProcTable::from_rows(rows);
    for line in fs::read_to_string(dir.join("written.tsv")).unwrap().lines().filter(|l| !l.starts_with('#')) {
        let (p, t) = line.split_once('\t').unwrap();
        f.probe.written.insert(PathBuf::from(p), t.parse().unwrap());
    }
    for line in fs::read_to_string(dir.join("panes.tsv")).unwrap().lines().filter(|l| !l.starts_with('#')) {
        let c: Vec<&str> = line.split('\t').collect();
        f.panes.push((c[0].parse().unwrap(), c[1].parse().unwrap(), c[2].to_string(), c[3].to_string()));
    }
    f
}

/// A claude pane's records as they stood at `now`.
fn log_of(uuid: &str, now: u64) -> ClaudeLog {
    let mut log = ClaudeLog::default();
    let path = fixtures().join("records").join(format!("main-{}.jsonl", &uuid[..8]));
    for line in fs::read_to_string(path).unwrap_or_default().lines() {
        if super::super::records::parse(line).is_some_and(|r| r.at() <= now) {
            log.feed_main(line);
        }
    }
    log
}

/// Which pane an outside process is, by the order the shell places it:
/// codex thread, claude session, pane token.  The token's check value
/// needs the secret of the machine that issued it, so the replay reads
/// only its pane number.
fn place(f: &Fixture, pid: i32) -> Option<(u64, String)> {
    let e = f.env.get(&pid)?;
    if let Some(sid) = e.thread.as_ref().and_then(|t| f.panes.iter().find(|p| p.3 == *t)).map(|p| p.0) {
        return Some((sid, "codex".into()));
    }
    if let Some(sid) = e.session.as_ref().and_then(|s| f.panes.iter().find(|p| p.3 == *s)).map(|p| p.0) {
        return Some((sid, "环境变量 CLAUDE_CODE_SESSION_ID 是这个会话".into()));
    }
    let sid = u64::from_str_radix(e.token.as_ref()?.split('-').next()?, 16).ok()?;
    Some((sid, "窗格令牌是这个窗格".into()))
}

struct Run {
    hits: Vec<(Rule, u64, i32)>,
}

fn run(f: &Fixture, watch: &mut Watch, probe: &mut MapProbe, now: u64) -> Run {
    let claude: Vec<&(u64, i32, String, String)> = f.panes.iter().filter(|p| p.2 == "claude").collect();
    let mut in_pane: HashSet<i32> = HashSet::new();
    for p in &f.panes {
        in_pane.insert(p.1);
        in_pane.extend(f.table.descendants(p.1).iter().map(|r| r.pid));
    }
    let logs: Vec<(u64, i32, ClaudeLog)> = claude
        .iter()
        .filter_map(|p| {
            let agent = f.table.children(p.1).next()?.pid;
            Some((p.0, agent, log_of(&p.3, now)))
        })
        .collect();
    let panes = logs.iter().map(|(sid, agent, log)| PaneIn { sid: *sid, agent: f.table.get(*agent).unwrap(), log }).collect();
    let mut outside = Vec::new();
    for r in f.table.rows().iter().filter(|r| r.pid > 1 && !in_pane.contains(&r.pid)) {
        let group: Vec<_> = f.table.rows().iter().filter(|o| o.pgid == r.pgid && o.pid != r.pid && !in_pane.contains(&o.pid)).collect();
        let placed = place(f, r.pid).or_else(|| group.iter().find_map(|g| place(f, g.pid)));
        let Some((sid, why)) = placed else { continue };
        let Some(pane) = claude.iter().find(|p| p.0 == sid) else { continue };
        let session = std::iter::once(r.pid).chain(group.iter().map(|g| g.pid)).find_map(|p| f.env[&p].session.clone());
        let gone = session.is_some_and(|s| !claude.iter().any(|p| p.3 == s));
        outside.push(OutsideIn { sid: pane.0, row: r, group, session_gone: gone, why });
    }
    let scene = Scene { now, table: &f.table, panes, outside, uid: 501, tmpdir: None, helper: &helper };
    let hits = watch.hits(&scene, probe);
    let pid = |id: &str| id.split('@').next().unwrap().parse().unwrap();
    Run { hits: hits.iter().map(|h| (h.key.rule, h.key.sid, pid(&h.key.id))).collect() }
}

#[test]
fn zombie_replay_of_the_scene_hits_the_three_dead_tails_and_nothing_else() {
    let mut f = load();
    let mut probe = std::mem::take(&mut f.probe);
    let got = run(&f, &mut Watch::default(), &mut probe, f.now).hits;
    // O1: the three adopted `tail -f` groups, their files 2.7 to 3.1 days unwritten
    //   488: tail of a log in its session's scratchpad (its own environment is
    //        unreadable; its ugrep's session places it)
    //   432: the two `tail -F` of logs in a project directory
    // O2: none.  The seven servers and the vite preview run from the project
    //     directories, not from a temporary place, and the session that started them
    //     still runs in a pane
    // B1: not judged.  CPU over 30 minutes needs a second check; one scene has one
    // B3: none.  No running task of pane 488 or pane 903 had its end notified
    // S1, S2: from records, see `history`
    assert_eq!(got, vec![(Rule::O1, 432, 47295), (Rule::O1, 432, 47436), (Rule::O1, 488, 35278)]);
}

#[test]
fn zombie_replay_b1_with_the_missing_readings_supplied_names_only_the_older_identical_watchdog() {
    let mut f = load();
    let mut probe = std::mem::take(&mut f.probe);
    // supplied, not measured: each watchdog of pane 488 had its output last written when
    // it started, as the newest one's was (b702piafb: written 16:30:50, started 16:30:50);
    // each watchdog of panes 488 and 903 spending 0.2 s of CPU in 30 minutes, the rate 44245
    // measured (2.7 s in 7.5 h)
    let tasks = "/private/tmp/claude-501/-Users-user-work-proj-a/153c990a-bc0f-49d0-9303-1bf639e4128e/tasks/";
    for (pid, task) in [(36160, "b6lpc8u81"), (66760, "bwsg7d7u5"), (69823, "b6ptg4btk")] {
        let started = f.table.get(pid).unwrap().start_unix;
        probe.written.insert(PathBuf::from(format!("{tasks}{task}.output")), started);
    }
    let roots = [36160, 44244, 66760, 69823, 49795, 71749];
    let mut w = Watch::default();
    for (i, at) in [f.now - 1800, f.now].into_iter().enumerate() {
        for pid in roots {
            probe.cpu.insert(pid, 1_000_000_000 + i as u64 * 200_000_000);
        }
        let got: Vec<_> = run(&f, &mut w, &mut probe, at).hits.into_iter().filter(|h| h.0 == Rule::B1).collect();
        if i == 0 {
            assert!(got.is_empty(), "one sample is no window: {got:?}");
        } else {
            // b6lpc8u81 and b702piafb ran `bash watchdog.sh --stopped 3600 --wake 2700`
            // word for word; the other two watchdogs of pane 488 began with a different
            // log call each; the pair of pane 903 differ in --wake and --stopped
            assert_eq!(got, vec![(Rule::B1, 488, 36160)]);
        }
    }
}

#[test]
fn zombie_replay_z0_counts_the_relays_under_the_shell() {
    let mut z = Vec::new();
    for line in fs::read_to_string(fixtures().join("zombies.tsv")).unwrap().lines().filter(|l| !l.starts_with('#')) {
        let (pid, ppid) = line.split_once('\t').unwrap();
        z.push(Zombie { pid: pid.parse().unwrap(), ppid: ppid.parse().unwrap(), start_us: 0 });
    }
    assert_eq!(rules::z0(&z, 1708, |_| false), vec![(1708, 56)]);
}
