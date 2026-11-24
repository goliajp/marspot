//! Binding panes to Codex threads, and the inbox and registry record
//! each binding owns: created when a pane's thread is certain, removed
//! when the pane, its process or its thread goes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use marspot::pidtree;
use marspot_peer::envelope::Mode;
use marspot_peer::json::{obj, Value};
use marspot_peer::registry::{self, Record, Status};
use marspot_peer::rpc::RpcError;
use marspot_term::{lx_info, lx_warn};

use marspot_peer::frame::IdleState;

use super::bind::{self, Pane};
use super::deliver::Rate;
use super::inbox::Inbox;
use super::idle::send_notices;
use super::others;
use super::records::private_dir;
use super::worker::{now_ms, write_record, Binding, Worker, RETRY_MIN};

/// How old a view of the process table a binding may be decided on.
const LOOK_AGE: Duration = Duration::from_millis(250);

impl Worker {
    /// Note which Codex processes are running, in panes or not, from a
    /// process table no older than `max_age`.
    pub(super) fn look_at_processes(&mut self, max_age: Duration) {
        let obs = crate::observe::shared().fresh(max_age);
        let mut homes: Vec<PathBuf> = std::env::var_os("HOME")
            .map_or(Vec::new(), |h| super::codex_config::config_files(Path::new(&h)))
            .iter()
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();
        homes.extend(self.wants.iter().filter_map(|w| crate::plugins::codex::home_of(w.codex.pid)));
        let daemons: Vec<pidtree::ProcId> = homes.iter().flat_map(|h| others::daemons_of(h)).collect();
        let oldest = self.wants.iter().map(|w| w.codex.start_us / 1000).min();
        self.codexes.look(&obs.table, &daemons, crate::plugins::beside::now_ms(), oldest);
        // the geminis too: telling one takes its command line, which
        // its own plugin, on the main thread, does not read for every
        // process there is
        if let Some(home) = std::env::var_os("HOME") {
            crate::plugins::gemini::look_at_processes(&obs.table, &mut self.geminis, Path::new(&home));
        }
    }

    pub(super) fn refresh(&mut self) {
        self.look_at_processes(LOOK_AGE);
        let mut panes = Vec::new();
        let mut procs: HashMap<u64, (pidtree::ProcId, Vec<String>)> = HashMap::new();
        self.follow_titles();
        for w in &self.wants {
            // the observation may be two seconds old
            if pidtree::proc_id(w.codex.pid) != Some(w.codex) {
                continue;
            }
            let (Some(home), Some(cwd)) = (crate::plugins::codex::home_of(w.codex.pid), pidtree::proc_cwd(w.codex.pid))
            else {
                continue;
            };
            let home = home.canonicalize().unwrap_or(home);
            let argv_thread = crate::plugins::codex::argv_thread_id(w.codex.pid);
            let cwd = cwd.to_string_lossy().into_owned();
            let started_ms = w.codex.start_us / 1000;
            panes.push(Pane {
                sid: w.sid,
                title_expected: super::title::title_expected(&home, &w.argv),
                home,
                alone: self.codexes.alone(w.codex, &cwd, started_ms),
                cwd,
                argv_thread,
                title_thread: w.title_thread.clone(),
                title_moved: self.titles[&w.sid].moved,
                started_ms,
                up_ms: (now_ms() as u64).saturating_sub(started_ms),
            });
            procs.insert(w.sid, (w.codex, w.argv.clone()));
        }

        let homes: HashSet<PathBuf> = panes.iter().map(|p| p.home.clone()).collect();
        let mut threads = HashMap::new();
        let mut failed = HashSet::new();
        for home in homes {
            match self.loaded_threads(&home) {
                Ok(ts) => {
                    threads.insert(home, ts);
                }
                Err(e) => {
                    lx_warn!("peerbus.daemon.unreachable", &e.to_string(), home = home.display().to_string());
                    failed.insert(home);
                }
            }
        }

        // a binding is its process's: the next Codex in the pane does
        // not inherit the thread of the one before
        let current: HashMap<u64, String> = self
            .bindings
            .iter()
            .filter(|(sid, b)| procs.get(*sid).is_some_and(|(codex, _)| *codex == b.codex))
            .map(|(sid, b)| (*sid, b.thread.clone()))
            .collect();
        let plan = bind::assign(&panes, &threads, &current);
        self.wait_for_titles(panes.iter().filter(|p| plan.get(&p.sid) == Some(&Err(bind::Unbound::AwaitingTitle))));

        let sids: Vec<u64> = self.bindings.keys().copied().collect();
        for sid in sids {
            let b = &self.bindings[&sid];
            if failed.contains(&b.home) {
                continue; // a daemon we could not ask is not a daemon that said no
            }
            let same_thread = matches!(plan.get(&sid), Some(Ok(t)) if *t == b.thread);
            let same_proc = procs.get(&sid).map(|(c, _)| *c) == Some(b.codex);
            if !(same_thread && same_proc) {
                let b = self.bindings.remove(&sid).expect("listed above");
                self.unbind(sid, b);
            }
        }
        for p in &panes {
            if self.bindings.contains_key(&p.sid) {
                continue;
            }
            match plan.get(&p.sid) {
                Some(Ok(thread)) => {
                    self.unbound.remove(&p.sid);
                    let (codex, argv) = procs[&p.sid].clone();
                    if let Err(e) = self.bind(p, thread, codex, &argv) {
                        lx_warn!("peerbus.bind.failed", &e, sid = p.sid);
                    }
                }
                Some(Err(why)) if self.unbound.get(&p.sid) != Some(why) => {
                    lx_info!("peerbus.unbound", &format!("{why:?}"), sid = p.sid, cwd = p.cwd.clone());
                    self.unbound.insert(p.sid, why.clone());
                }
                Some(Err(_)) => {}
                None => {}
            }
        }
        let live: HashSet<u64> = panes.iter().map(|p| p.sid).collect();
        self.unbound.retain(|sid, _| live.contains(sid));
        if failed.is_empty() {
            self.backoff = RETRY_MIN;
        } else {
            self.schedule_retry();
        }
    }

    /// The daemon's loaded user threads with their cwds.
    pub(super) fn loaded_threads(&mut self, home: &Path) -> Result<Vec<(String, String)>, RpcError> {
        let r = self.call(home, "thread/loaded/list", obj([]))?;
        let ids: Vec<String> = r.get("data").map_or(&[][..], Value::arr).iter().filter_map(Value::str).map(str::to_string).collect();
        self.cwds.retain(|(h, id), _| h != home || ids.contains(id));
        let mut out = Vec::new();
        for id in ids {
            let key = (home.to_path_buf(), id.clone());
            if !self.cwds.contains_key(&key) {
                let read = self.call(home, "thread/read", obj([("threadId", Value::from(id.as_str())), ("includeTurns", Value::from(false))]));
                match read {
                    Ok(v) => {
                        let cwd = v.get("thread").and_then(bind::user_thread_cwd);
                        self.cwds.insert(key.clone(), cwd);
                    }
                    Err(RpcError::Remote { .. }) => continue, // unloaded in between
                    Err(e) => return Err(e),
                }
            }
            if let Some(Some(cwd)) = self.cwds.get(&key) {
                out.push((id, cwd.clone()));
            }
        }
        Ok(out)
    }

    /// The thread's permission class, and whether turn events will now
    /// arrive for it.  `thread/resume` answers for a thread that has run
    /// a turn; before that it fails, and the class comes from the
    /// effective config plus the TUI's own command line.
    pub(super) fn class_for(&mut self, home: &Path, thread: &str, cwd: &str, argv: &[String]) -> (Mode, bool) {
        let resume = obj([("threadId", Value::from(thread)), ("excludeTurns", Value::from(true))]);
        match self.call(home, "thread/resume", resume) {
            Ok(r) => (bind::class_of(r.get("approvalPolicy"), r.get("sandbox")), true),
            Err(RpcError::Remote { .. }) => {
                let o = bind::argv_override(argv);
                if o.opaque {
                    return (Mode::Prompting, false);
                }
                let cfg = self.call(home, "config/read", obj([("cwd", Value::from(cwd))])).ok();
                let c = cfg.as_ref().and_then(|r| r.get("config"));
                let approval = o.approval.map(Value::from).or_else(|| c.and_then(|c| c.get("approval_policy")).cloned());
                let sandbox = o.sandbox.map(Value::from).or_else(|| c.and_then(|c| c.get("sandbox_mode")).cloned());
                (bind::class_of(approval.as_ref(), sandbox.as_ref()), false)
            }
            Err(_) => (Mode::Prompting, false),
        }
    }

    pub(super) fn bind(&mut self, p: &Pane, thread: &str, codex: pidtree::ProcId, argv: &[String]) -> Result<(), String> {
        let (class, subscribed) = self.class_for(&p.home, thread, &p.cwd, argv);
        let r = self.bind_with(p, thread, codex, argv, class, subscribed);
        if r.is_err() && subscribed {
            self.unsubscribe(&p.home, thread);
        } else if r.is_ok() {
            self.moved_in(p.sid, codex);
        }
        r
    }

    fn bind_with(
        &mut self,
        p: &Pane,
        thread: &str,
        codex: pidtree::ProcId,
        argv: &[String],
        class: Mode,
        subscribed: bool,
    ) -> Result<(), String> {
        let version = self.conns.get(&p.home).map(|c| c.version.clone()).unwrap_or_default();
        let dir = self.sock_dir().map_err(|e| format!("socket directory: {e}"))?;
        let private = private_dir().map_err(|e| format!("private socket directory: {e}"))?;
        let key = self.next_key;
        self.next_key += 1;
        let inbox = Inbox::open(&dir, &private, codex.pid, key, self.tx.clone()).map_err(|e| format!("inbox: {e}"))?;
        // the record names the relay: it is the process on the inbox
        let Some(proc_start) = marspot_peer::proc::lstart(inbox.relay_pid()) else {
            inbox.close();
            return Err("the relay exited".into());
        };
        let dirs = registry::default_dirs();
        // the pane's name, as everywhere; the names check keeps it so
        let name = super::names::Panes::look()
            .names
            .remove(&p.sid)
            .unwrap_or_else(|| marspot::pane_name::base_name(&p.cwd));
        let record = Record {
            pid: inbox.relay_pid(),
            agent: registry::Agent::Codex,
            agent_pid: codex.pid,
            relay: inbox.relay.to_string_lossy().into_owned(),
            session_id: thread.to_string(),
            cwd: p.cwd.clone(),
            started_at_ms: now_ms(),
            proc_start,
            version,
            name,
            name_since_ms: now_ms(),
            socket: inbox.path.to_string_lossy().into_owned(),
            status: Status::Idle,
            class,
            owner: self.owner.clone(),
            spool: None,
        };
        lx_info!(
            "peerbus.bind",
            "codex session has an inbox",
            sid = p.sid,
            thread = thread.to_string(),
            name = record.name.clone(),
            class = class.as_str(),
            subscribed = subscribed
        );
        let b = Binding {
            key,
            codex,
            home: p.home.clone(),
            thread: thread.to_string(),
            class,
            argv: argv.to_vec(),
            subscribed,
            active_turn: None,
            inbox,
            record,
            rate: Rate::default(),
            subs: Vec::new(),
            last_delivered: None,
            replies: Default::default(),
        };
        let record = b.record.clone();
        self.bindings.insert(p.sid, b);
        // a call held for the record is asked about by its thread next
        self.publish();
        write_record(&dirs, &record);
        Ok(())
    }

    pub(super) fn unbind(&mut self, sid: u64, mut b: Binding) {
        self.moved_out(sid);
        send_notices(sid, &mut b, IdleState::Exited);
        super::records::remove_record(&b.record);
        if b.subscribed {
            self.unsubscribe(&b.home, &b.thread);
        }
        lx_info!("peerbus.unbind", "inbox removed", thread = b.thread.clone(), name = b.record.name.clone());
        b.inbox.close();
    }

    /// Drop this connection's subscription.  Codex unloads a thread
    /// only once it has had no subscriber for half an hour; a
    /// subscription left behind would keep a quit TUI's thread loaded
    /// for as long as marspot runs, and leave its directory ambiguous
    /// for the next Codex started there.
    pub(super) fn unsubscribe(&mut self, home: &Path, thread: &str) {
        if !self.conns.contains_key(home) {
            return; // the connection is gone, and its subscriptions with it
        }
        if let Err(e) = self.call(home, "thread/unsubscribe", obj([("threadId", Value::from(thread))])) {
            lx_warn!("peerbus.unsubscribe_failed", &e.to_string(), thread = thread.to_string());
        }
    }
}
