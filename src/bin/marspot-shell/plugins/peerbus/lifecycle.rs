//! Binding panes to Codex threads, and the inbox and registry record
//! each binding owns: created when a pane's thread is certain, removed
//! when the pane, its process or its thread goes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use marspot::pidtree;
use marspot_peer::envelope::Mode;
use marspot_peer::json::{obj, Value};
use marspot_peer::registry::{self, Record, Status};
use marspot_peer::rpc::RpcError;
use marspot_term::{lx_info, lx_warn};

use super::bind::{self, Pane};
use super::deliver::Rate;
use super::inbox::Inbox;
use super::worker::{is_inbox_name, now_ms, write_record, Binding, Worker, RETRY_MIN};

impl Worker {
    pub(super) fn refresh(&mut self) {
        let mut panes = Vec::new();
        let mut procs: HashMap<u64, (pidtree::ProcId, Vec<String>)> = HashMap::new();
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
            panes.push(Pane {
                sid: w.sid,
                home,
                cwd: cwd.to_string_lossy().into_owned(),
                argv_thread,
                title_thread: w.title_thread.clone(),
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

        let current: HashMap<u64, String> = self.bindings.iter().map(|(sid, b)| (*sid, b.thread.clone())).collect();
        let plan = bind::assign(&panes, &threads, &current);

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
                self.unbind(b);
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
        let proc_start = marspot_peer::proc::lstart(codex.pid).ok_or("codex exited")?;
        let dir = self.sock_dir().map_err(|e| format!("socket directory: {e}"))?;
        let key = self.next_key;
        self.next_key += 1;
        let inbox = Inbox::open(&dir, codex.pid, key, self.tx.clone()).map_err(|e| format!("inbox: {e}"))?;
        let dirs = registry::default_dirs();
        let taken: Vec<String> = dirs
            .iter()
            .flat_map(|d| registry::list_live(d))
            .filter(|e| e.pid != codex.pid)
            .filter_map(|e| e.name)
            .collect();
        let record = Record {
            pid: codex.pid,
            session_id: thread.to_string(),
            cwd: p.cwd.clone(),
            started_at_ms: now_ms(),
            proc_start,
            version,
            name: bind::record_name(&p.cwd, thread, &taken),
            socket: inbox.path.to_string_lossy().into_owned(),
            status: Status::Idle,
            class,
            owner: self.owner.clone(),
        };
        write_record(&dirs, &record);
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
        };
        self.bindings.insert(p.sid, b);
        Ok(())
    }

    pub(super) fn unbind(&mut self, b: Binding) {
        for d in registry::default_dirs() {
            let _ = registry::remove_ours(&d, b.codex.pid);
        }
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

    /// Claude Code checks that a reply address is in the same directory
    /// as its own socket, so the inbox goes where live Claude sessions
    /// put theirs.
    pub(super) fn sock_dir(&mut self) -> std::io::Result<PathBuf> {
        if let Some(d) = &self.sock_dir {
            return Ok(d.clone());
        }
        let from_claude = registry::default_dirs()
            .iter()
            .flat_map(|d| registry::list_live(d))
            .filter(|e| !e.ours)
            .find_map(|e| e.socket.and_then(|s| Path::new(&s).parent().map(Path::to_path_buf)));
        let dir = from_claude.unwrap_or_else(|| PathBuf::from("/tmp/cc-socks"));
        if !dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
        }
        self.sock_dir = Some(dir.clone());
        Ok(dir)
    }

    /// Records a previous run of this marspot left (an execv'd shell
    /// does not stop its plugins), and their dead sockets.
    pub(super) fn clear_leftovers(&mut self) {
        for d in registry::default_dirs() {
            for e in registry::remove_owned_by(&d, &self.owner) {
                if let Some(sock) = e.socket.filter(|s| is_inbox_name(s, e.pid)) {
                    let _ = std::fs::remove_file(sock);
                }
            }
            registry::sweep_ours(&d);
        }
    }

    pub(super) fn teardown(&mut self) {
        let sids: Vec<u64> = self.bindings.keys().copied().collect();
        for sid in sids {
            if let Some(b) = self.bindings.remove(&sid) {
                self.unbind(b);
            }
        }
        for c in self.conns.values() {
            c.tx.shutdown();
        }
        self.conns.clear();
    }
}
