//! One check of the watch, with everything it keeps between checks.
//! Runs on the watch's own thread (`plugin`): it reads transcripts and
//! stats files, which may block for seconds on a network mount.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use marspot::lx_info;
use marspot::pidtree::ProcTable;
use marspot::zombie::check::{Key, Scene, Suspect, Watch};
use marspot::zombie::files::PaneFiles;
use marspot::zombie::limits::{self, Notify};
use marspot::zombie::probe::Live;
use marspot::zombie::{rules, tell};

use super::attrib::{Attributor, Known};
use super::{dryrun, scene, store};

pub(super) struct Checker {
    /// `<state>/zombie`.
    dir: PathBuf,
    attrib: Attributor,
    probe: Live,
    watch: Watch,
    files: HashMap<u64, PaneFiles>,
    pacer: tell::Pacer,
    /// Suspects already logged, while what they name lives.
    logged: HashSet<Key>,
    /// Panes whose due message the pacing held back, as last logged.
    held: HashSet<u64>,
    unreaped: Vec<(i32, usize)>,
}

impl Checker {
    /// A checker keeping its state in `dir`, going on from the pacing
    /// saved there.
    pub fn new(dir: PathBuf, now: u64) -> Checker {
        let pacer = store::load(&dir, now);
        Checker {
            dir,
            attrib: Attributor::default(),
            probe: Live::default(),
            watch: Watch::default(),
            files: HashMap::new(),
            pacer,
            logged: HashSet::new(),
            held: HashSet::new(),
            unreaped: Vec::new(),
        }
    }

    pub fn check(&mut self, now: u64) {
        let body = std::fs::read_to_string(marspot::settings::path()).unwrap_or_default();
        let (notify, l) = limits::parse(&body);
        if notify == Notify::Off {
            return;
        }
        self.watch.limits = l.clone();
        let obs = crate::observe::shared().latest();
        let known = Known::current();
        self.attrib.begin(&obs, &known);
        self.probe.prune(&obs.table);
        let in_pane = scene::in_panes(&obs);
        self.z0(&in_pane);
        let agents = scene::refresh(&obs, &known, &mut self.files);
        let outside = scene::outside(&obs, &in_pane, &known, &mut self.attrib, now, &l);
        let helper = crate::plugins::claudecode::is_resting_helper;
        let s = Scene {
            now,
            table: &obs.table,
            panes: scene::panes(&obs, &agents, &self.files),
            outside,
            // SAFETY: getuid cannot fail
            uid: unsafe { libc::getuid() },
            tmpdir: std::env::var_os("TMPDIR").map(Into::into),
            helper: &helper,
        };
        let suspects = self.watch.check(&s, &mut self.probe);
        let log = self.dir.join("dryrun.log");
        for x in &suspects {
            if self.logged.insert(x.hit.key.clone()) {
                let k = &x.hit.key;
                lx_info!("zombie.suspect", "a stable suspect; evidence in zombie/dryrun.log", sid = k.sid, rule = k.rule.name(), id = k.id.as_str(), since = x.since);
                let head = format!("sid={} rule={} id={} since={}", k.sid, k.rule.name(), k.id, x.since);
                dryrun::append(&log, "suspect", now, &head, &x.hit.evidence);
            }
        }
        let told = self.would_tell(&suspects, now);
        let open: HashSet<u64> = known.claude.iter().map(|c| c.sid).collect();
        let before = (self.pacer.told_tsv(), self.pacer.clock_tsv());
        let alive = |id: &str| alive(id, &obs.table, &self.files);
        self.logged.retain(|k| alive(&k.id));
        self.pacer.keep_alive(alive);
        self.pacer.keep_panes(|sid| open.contains(&sid));
        self.held.retain(|sid| open.contains(sid));
        if told || before != (self.pacer.told_tsv(), self.pacer.clock_tsv()) {
            store::save(&self.dir, &self.pacer);
        }
    }

    /// What would be sent, and when the pacing would hold it back.
    /// Whether anything would have gone out.
    fn would_tell(&mut self, suspects: &[Suspect], now: u64) -> bool {
        let mut by_pane: HashMap<u64, Vec<&Suspect>> = HashMap::new();
        for s in suspects.iter().filter(|s| !self.pacer.told(&s.hit.key.id)) {
            by_pane.entry(s.hit.key.sid).or_default().push(s);
        }
        let mut any = false;
        for (sid, items) in by_pane {
            let ended = self.files.get(&sid).and_then(|f| f.log.main.turn_ended);
            if !tell::due(&items, &self.pacer, ended, now) {
                continue;
            }
            if !self.pacer.allows(sid, now) {
                if self.held.insert(sid) {
                    lx_info!("zombie.would_tell.held", "the pacing holds this pane's message back", sid = sid, items = items.len());
                }
                continue;
            }
            self.held.remove(&sid);
            let ids: Vec<&str> = items.iter().map(|s| s.hit.key.id.as_str()).collect();
            lx_info!("zombie.would_tell", "a message this dry run did not send; text in zombie/dryrun.log", sid = sid, items = ids.join(","));
            let head = format!("sid={sid} items={}", ids.join(","));
            dryrun::append(&self.dir.join("dryrun.log"), "would_tell", now, &head, &tell::body(&items));
            self.pacer.record(sid, now, items.iter().map(|s| s.hit.key.id.clone()));
            any = true;
        }
        any
    }

    /// Z0: unreaped children of the shell and of the panes' processes,
    /// logged when the counts change.
    fn z0(&mut self, in_pane: &HashSet<i32>) {
        let counts = rules::z0(&marspot::pidtree::zombies(), std::process::id() as i32, |p| in_pane.contains(&p));
        if counts != self.unreaped {
            let total: usize = counts.iter().map(|(_, n)| n).sum();
            let per: Vec<String> = counts.iter().map(|(p, n)| format!("{p}:{n}")).collect();
            lx_info!("zombie.unreaped", &per.join(" "), total = total);
            self.unreaped = counts;
        }
    }
}

/// What item `id` names still lives: the process `pid@start_us` runs,
/// or the subagent `agent:<id>` is of a pane's claude process and not
/// finished.
fn alive(id: &str, table: &ProcTable, files: &HashMap<u64, PaneFiles>) -> bool {
    if let Some(sub) = id.strip_prefix("agent:") {
        return files.values().any(|f| f.has_sub(sub));
    }
    let Some((pid, start)) = id.split_once('@') else { return false };
    let (Ok(pid), Ok(start)) = (pid.parse::<i32>(), start.parse::<u64>()) else { return false };
    table.get(pid).is_some_and(|r| r.start_us == start)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn zombie_checks_walk_no_process_table_of_their_own() {
        let obs = crate::observe::shared();
        obs.fresh(crate::observe::NOW);
        let before = marspot::pidtree::table_walks();
        let started = Instant::now();
        let dir = std::env::temp_dir().join(format!("zombie-checker-{}", std::process::id()));
        let mut c = Checker::new(dir.clone(), 1_800_000_000);
        for i in 0..100 {
            c.check(1_800_000_000 + i);
        }
        let walks = marspot::pidtree::table_walks() - before;
        // the observer's own cadence may land one walk inside the run
        let cadence = (started.elapsed().as_secs_f64() / crate::observe::INTERVAL.as_secs_f64()).ceil() as u64;
        assert!(walks <= cadence, "{walks} walks in {:?} of 100 checks", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zombie_an_item_lives_while_its_process_runs_with_the_same_start() {
        let me = std::process::id() as i32;
        let table = ProcTable::take();
        let start = table.get(me).unwrap().start_us;
        let files = HashMap::new();
        assert!(alive(&format!("{me}@{start}"), &table, &files));
        assert!(!alive(&format!("{me}@{}", start + 1), &table, &files), "another process with the pid");
        assert!(!alive("agent:a1", &table, &files), "no pane reads that subagent");
        assert!(!alive("junk", &table, &files));
    }
}
