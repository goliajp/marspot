//! One check of the watch, with everything it keeps between checks.
//! Runs on the watch's own thread (`plugin`): it reads transcripts and
//! stats files, which may block for seconds on a network mount.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use marspot::lx_info;
use marspot::pidtree::ProcTable;
use marspot::zombie::check::{Key, OutsideIn, PaneIn, Scene, Watch};
use marspot::zombie::files::PaneFiles;
use marspot::zombie::limits::{self, Notify};
use marspot::zombie::orphans::{self as record, Orphan};
use marspot::zombie::probe::Live;
use marspot::zombie::rules::{self, Limits};

use super::attrib::{Attributor, Known};
use super::teller::{Cx, Teller};
use super::{deliver, dryrun, orphans, plugin, scene, store};

pub(super) struct Checker {
    /// `<state>/zombie`.
    dir: PathBuf,
    attrib: Attributor,
    probe: Live,
    watch: Watch,
    files: HashMap<u64, PaneFiles>,
    pacers: store::Pacers,
    /// Suspects already logged, while what they name lives.
    logged: HashSet<Key>,
    teller: Teller,
    /// The mode and scale last logged.
    mode: Option<(Notify, u64)>,
    unreaped: Vec<(i32, usize)>,
    /// The panes seen lately, by name, and the helpers' parents.
    names: orphans::Names,
    /// The rules run over the processes closed panes left.
    orphan_watch: Watch,
    /// The record of those processes as last written.
    orphans: Vec<Orphan>,
    written: [String; 2],
}

impl Checker {
    /// A checker keeping its state in `dir`, going on from the pacing
    /// saved there.
    pub fn new(dir: PathBuf, now: u64) -> Checker {
        let pacers = store::load(&dir, now);
        let names = orphans::Names::load(&dir);
        let orphans = record::parse(&std::fs::read_to_string(record::path(dir.parent().unwrap_or(&dir))).unwrap_or_default());
        Checker {
            dir,
            attrib: Attributor::default(),
            probe: Live::default(),
            watch: Watch::default(),
            files: HashMap::new(),
            pacers,
            logged: HashSet::new(),
            teller: Teller::default(),
            mode: None,
            unreaped: Vec::new(),
            written: [names.tsv(), record::format(&orphans)],
            names,
            orphan_watch: Watch::default(),
            orphans,
        }
    }

    pub fn check(&mut self, now: u64) {
        let body = std::fs::read_to_string(marspot::settings::path()).unwrap_or_default();
        let (notify, l) = limits::parse(&body);
        if notify == Notify::Off {
            return;
        }
        let scale = if plugin::sandboxed() { limits::scale(&body) } else { 1 };
        plugin::set_scale(scale);
        if self.mode.replace((notify, scale)) != Some((notify, scale)) {
            lx_info!("zombie.mode", "the watch checks claude panes", notify = format!("{notify:?}"), scale = scale);
        }
        let l = l.scaled(scale);
        self.watch.limits = l.clone();
        self.watch.scale = scale;
        for p in self.pacers.both() {
            p.scale = scale;
        }
        let obs = crate::observe::shared().latest();
        let known = self.known(&obs, now);
        self.attrib.begin(&obs, &known);
        self.probe.prune(&obs.table);
        let in_pane = scene::in_panes(&obs);
        self.z0(&in_pane);
        let agents = scene::refresh(&obs, &known, &mut self.files);
        let outside = scene::outside(&obs, &in_pane, &known, &mut self.attrib, now, &l);
        let s = scene(now, &obs.table, scene::panes(&obs, &agents, &self.files), outside);
        let suspects = self.watch.check(&s, &mut self.probe);
        self.log_suspects(&suspects, now);
        self.orphans(&obs, &in_pane, &known, now, &l);
        let reach = crate::plugins::claudecode::reach().into_iter().map(|p| (p.sid, p)).collect();
        let files = &self.files;
        let turn_ended = |sid: u64| files.get(&sid).is_some_and(|f| f.log.main.turn_ended.is_some());
        let cx = Cx { notify, now, scale, dir: &self.dir, reach: &reach, turn_ended: &turn_ended };
        let told = self.teller.tell(&cx, &suspects, self.pacers.of(notify), &mut deliver::deliver);
        let open: HashSet<u64> = known.claude.iter().map(|c| c.sid).collect();
        let before = self.pacers.tsv();
        let alive = |id: &str| alive(id, &obs.table, &self.files);
        self.logged.retain(|k| alive(&k.id));
        for p in self.pacers.both() {
            p.keep_alive(alive);
            p.keep_panes(|sid| open.contains(&sid));
        }
        self.teller.keep_panes(|sid| open.contains(&sid));
        if told || before != self.pacers.tsv() {
            store::save(&self.dir, &self.pacers);
        }
    }

    /// The bindings of this check, with every helper's parent the watch
    /// remembers; notes the names of the panes running now.
    fn known(&mut self, obs: &crate::observe::Observation, now: u64) -> Known {
        let named = marspot::pane_name::assign(&crate::ShellApp::pane_refs_in(obs));
        self.names.note(named, &crate::plugins::helpers::strip::parents(), now);
        let mut known = Known::current();
        known.helper_parents = self.names.parents();
        known
    }

    fn log_suspects(&mut self, suspects: &[marspot::zombie::check::Suspect], now: u64) {
        let log = self.dir.join("dryrun.log");
        for x in suspects {
            if self.logged.insert(x.hit.key.clone()) {
                let k = &x.hit.key;
                lx_info!("zombie.suspect", "a stable suspect; evidence in zombie/dryrun.log", sid = k.sid, rule = k.rule.name(), id = k.id.as_str(), since = x.since);
                let head = format!("sid={} rule={} id={} since={}", k.sid, k.rule.name(), k.id, x.since);
                dryrun::append(&log, "suspect", now, &head, &x.hit.evidence);
            }
        }
    }

    /// Record the processes closed panes left, after the claude panes'
    /// own placements have had this check's reads.
    fn orphans(&mut self, obs: &crate::observe::Observation, in_pane: &HashSet<i32>, known: &Known, now: u64, l: &Limits) {
        let (found, later) = orphans::closed(obs, in_pane, known, &mut self.attrib);
        let s = scene(now, &obs.table, Vec::new(), found);
        self.orphan_watch.limits = l.clone();
        self.orphan_watch.scale = self.watch.scale;
        self.orphans = orphans::record(&self.orphans, &s.outside, &later, &s, &mut self.orphan_watch, &mut self.probe, &self.names);
        let now = [self.names.tsv(), record::format(&self.orphans)];
        if now == self.written {
            return;
        }
        let state = self.dir.parent().unwrap_or(&self.dir).to_path_buf();
        let wrote = store::ensure(&self.dir)
            .and_then(|()| store::write(&self.dir.join("names.tsv"), &now[0]))
            .and_then(|()| store::write(&record::path(&state), &now[1]));
        match wrote {
            Ok(()) => self.written = now,
            Err(e) => marspot::lx_warn!("zombie.orphans.unwritten", &format!("{}: {e}", self.dir.display())),
        }
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

fn scene<'a>(now: u64, table: &'a ProcTable, panes: Vec<PaneIn<'a>>, outside: Vec<OutsideIn<'a>>) -> Scene<'a> {
    Scene {
        now,
        table,
        panes,
        outside,
        // SAFETY: getuid cannot fail
        uid: unsafe { libc::getuid() },
        tmpdir: std::env::var_os("TMPDIR").map(Into::into),
        helper: &crate::plugins::claudecode::is_resting_helper,
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
