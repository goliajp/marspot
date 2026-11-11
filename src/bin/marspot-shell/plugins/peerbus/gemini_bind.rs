//! Gemini panes as peers: the registry record and inbox each one owns,
//! and what happens to a message that arrives there.
//!
//! A Gemini pane is bound once the gemini plugin has named its session
//! for certain, and never on less.  The record and the inbox are the
//! ones a Codex pane gets.  Delivery is not: nothing outside a running
//! Gemini can put text into its session, and typing into its composer
//! would land in the middle of whatever the user is typing.  So a
//! message is kept in a file (`marspot_peer::spool`), named in the
//! record, and Gemini's own hook collects it at the session's next
//! turn boundary (`marspot_peer::gemini_hook`, installed by
//! `gemini_config`).
//!
//! The limit that follows: a message does not wake an idle Gemini.  It
//! is read when the user next sends a prompt there, or when the turn
//! that is running next calls a tool, whichever comes first.  The
//! sender is told the same thing as for any peer, that the message is
//! in the inbox and not yet read.
//!
//! Nothing reports when a Gemini session goes idle either, so its
//! record offers no idle notice and carries no status, and a
//! subscription that arrives anyway is not held: no notice is ever
//! made up.
//!
//! Kimi panes are bound the same way and their mail waits the same
//! way; what differs for them is in `kimi_bind`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use marspot::pidtree;
use marspot_peer::envelope::Mode;
use marspot_peer::frame::{DropReason, Receipt, UserFrame};
use marspot_peer::registry::{self, Agent, Record, Status};
use marspot_peer::spool;
use marspot_term::{lx_info, lx_warn};

use super::deliver::{self, Rate};
use super::inbox::Inbox;
use super::records::private_dir;
use super::worker::{now_ms, write_record, Worker};
use crate::plugins::gemini::{Peer, Peers};

pub(super) struct GeminiBinding {
    /// Unique per inbox, among the Codex bindings' keys too.
    pub(super) key: u64,
    pub(super) gemini: pidtree::ProcId,
    pub(super) class: Mode,
    pub(super) inbox: Inbox,
    /// Its `session_id` is the Gemini session the pane is in.
    pub(super) record: Record,
    pub(super) rate: Rate,
    /// Where this session's mail waits.
    pub(super) spool: PathBuf,
    /// Idle subscriptions, for an agent whose record offers the
    /// notice: (reply address, subscription msg_id).
    pub(super) subs: Vec<(String, String)>,
    /// The session's last ended turn, where the agent's record numbers
    /// them.
    pub(super) ended: Option<i64>,
}

impl GeminiBinding {
    /// Remove the record and the inbox.  The waiting mail goes with
    /// them only when the session's process has gone: a marspot that
    /// restarts binds the same session again and finds it there.
    pub(super) fn unbind(mut self, session_gone: bool) {
        self.notify(marspot_peer::frame::IdleState::Exited);
        for d in registry::default_dirs() {
            let _ = registry::remove_ours(&d, self.record.pid);
        }
        if session_gone {
            let _ = std::fs::remove_file(&self.spool);
        }
        lx_info!("peerbus.gemini.unbind", "inbox removed", session = self.record.session_id.clone(), name = self.record.name.clone());
        self.inbox.close();
    }
}

/// The sockets and files of one binding, as the record names them.
struct Endpoint {
    relay_pid: i32,
    proc_start: String,
    socket: String,
    relay: String,
    spool: String,
}

/// The record for the Gemini session of `p`.
fn record_for(p: &Peer, at: Endpoint, name: String, class: Mode, owner: &str, now: i64) -> Record {
    Record {
        pid: at.relay_pid,
        agent: Agent::Gemini,
        agent_pid: p.gemini.pid,
        relay: at.relay,
        session_id: p.session.clone(),
        cwd: p.cwd.clone(),
        started_at_ms: now,
        proc_start: at.proc_start,
        version: String::new(),
        name,
        name_since_ms: now,
        former_names: Vec::new(),
        socket: at.socket,
        status: Status::Unknown,
        class,
        owner: owner.to_string(),
        spool: Some(at.spool),
    }
}

/// The permission class a gemini command line asks for.  Only the
/// command line can turn approvals off for a whole session; the
/// settings file cannot.  A session switched to it from inside keeps
/// reading as prompting, which errs toward refusing a message.
fn class_of(argv: &[String]) -> Mode {
    let mut it = argv.iter().skip(1).map(String::as_str);
    while let Some(arg) = it.next() {
        let yolo = match arg {
            "-y" | "--yolo" | "--approval-mode=yolo" => true,
            "--approval-mode" => it.next() == Some("yolo"),
            _ => false,
        };
        if yolo {
            return Mode::Bypass;
        }
    }
    Mode::Prompting
}

/// Let `frame` in and keep it at `spool`, or say what the sender is to
/// be told instead.
fn keep(frame: &UserFrame, class: Mode, rate: &mut Rate, spool: &Path, cap: usize) -> Result<(), (Receipt, String)> {
    let admitted = deliver::admit(frame, class, rate, Instant::now()).map_err(|why| (why.receipt(), why.reason()))?;
    match spool::append(spool, &admitted.text, cap) {
        Ok(true) => Ok(()),
        // the words the full channel in front of the inbox uses
        Ok(false) => Err((Receipt::Dropped(DropReason::QueueFull), "recipient inbox is full".to_string())),
        Err(e) => Err((Receipt::Refused, format!("the message could not be kept for the session: {e}"))),
    }
}

/// Where this marspot keeps the mail of `agent`'s sessions.  One
/// directory per agent: each is swept against that agent's panes only.
fn spool_dir(agent: Agent) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = private_dir()?.join(if agent == Agent::Kimi { "kimi" } else { "gemini" });
    std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
    Ok(dir)
}

/// Remove every file in `dir` that is not the mail of a session in
/// `bound`: what sessions that have since ended left behind.
fn sweep(dir: &Path, bound: &HashSet<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if !bound.contains(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Worker {
    /// Bring the Gemini bindings in line with the panes.
    pub(super) fn apply_gemini(&mut self, peers: &Peers) {
        self.apply_spooled(Agent::Gemini, peers, |p| class_of(&pidtree::proc_argv(p.gemini.pid).unwrap_or_default()));
    }

    /// Bring the bindings of `agent`'s panes in line with `peers`;
    /// `class_of` says what a pane newly bound lets in.
    pub(super) fn apply_spooled(&mut self, agent: Agent, peers: &Peers, class_of: impl Fn(&Peer) -> Mode) {
        // the plugin's view may be a tick old
        let live: Vec<&Peer> = peers.bound.iter().filter(|p| pidtree::proc_id(p.gemini.pid) == Some(p.gemini)).collect();
        // a pane another agent's binding still holds runs this agent now
        let stale = |g: &GeminiBinding, sid: u64| match g.record.agent == agent {
            true => !live.iter().any(|p| p.sid == sid && p.gemini == g.gemini),
            false => live.iter().any(|p| p.sid == sid),
        };
        let sids: Vec<u64> = self.gemini.iter().filter(|(sid, g)| stale(g, **sid)).map(|(sid, _)| *sid).collect();
        for sid in sids {
            if let Some(g) = self.gemini.remove(&sid) {
                let gemini = g.gemini;
                g.unbind(pidtree::proc_id(gemini.pid) != Some(gemini));
            }
        }
        let dir = match spool_dir(agent) {
            Ok(dir) => dir,
            Err(e) => {
                lx_warn!("peerbus.gemini.spool_dir", &e.to_string());
                return;
            }
        };
        for p in live {
            match self.gemini.get_mut(&p.sid) {
                Some(g) if g.record.session_id == p.session => {}
                // `/clear` starts another session under the same process
                Some(g) => {
                    let to = spool::path_for(&dir, &p.session);
                    let _ = std::fs::rename(&g.spool, &to);
                    lx_info!("peerbus.gemini.session", "the pane moved to another session", sid = p.sid, session = p.session.clone());
                    g.spool = to;
                    g.record.session_id = p.session.clone();
                    g.record.spool = Some(g.spool.to_string_lossy().into_owned());
                    write_record(&registry::default_dirs(), &g.record);
                }
                None => {
                    if let Err(e) = self.bind_gemini(agent, p, class_of(p), &dir) {
                        lx_warn!("peerbus.gemini.bind_failed", &e, sid = p.sid, agent = agent.label());
                    }
                }
            }
        }
        if peers.all_known {
            sweep(&dir, &self.gemini.values().map(|g| g.spool.clone()).collect());
        }
    }

    fn bind_gemini(&mut self, agent: Agent, p: &Peer, class: Mode, spool_dir: &Path) -> Result<(), String> {
        if !spool::is_session_name(&p.session) {
            return Err("the session id cannot be a file name".into());
        }
        let dir = self.sock_dir().map_err(|e| format!("socket directory: {e}"))?;
        let private = private_dir().map_err(|e| format!("private socket directory: {e}"))?;
        let key = self.next_key;
        self.next_key += 1;
        let inbox = Inbox::open(&dir, &private, p.gemini.pid, key, self.tx.clone()).map_err(|e| format!("inbox: {e}"))?;
        let Some(proc_start) = marspot_peer::proc::lstart(inbox.relay_pid()) else {
            inbox.close();
            return Err("the relay exited".into());
        };
        let spool = spool::path_for(spool_dir, &p.session);
        let at = Endpoint {
            relay_pid: inbox.relay_pid(),
            proc_start,
            socket: inbox.path.to_string_lossy().into_owned(),
            relay: inbox.relay.to_string_lossy().into_owned(),
            spool: spool.to_string_lossy().into_owned(),
        };
        let name = super::names::Panes::look()
            .names
            .remove(&p.sid)
            .unwrap_or_else(|| marspot::pane_name::base_name(&p.cwd));
        let mut record = record_for(p, at, name, class, &self.owner, now_ms());
        record.agent = agent;
        write_record(&registry::default_dirs(), &record);
        lx_info!(
            "peerbus.gemini.bind",
            "session has an inbox",
            sid = p.sid,
            agent = agent.label(),
            session = p.session.clone(),
            name = record.name.clone(),
            class = class.as_str()
        );
        let subs = Vec::new();
        self.gemini.insert(p.sid, GeminiBinding { key, gemini: p.gemini, class, inbox, record, rate: Rate::default(), spool, subs, ended: None });
        Ok(())
    }

    /// A frame that reached a Gemini inbox: kept for the session's next
    /// turn, or refused with the receipt a Codex inbox would send.
    pub(super) fn deliver_gemini(&mut self, key: u64, frame: &UserFrame, peer_pid: Option<i32>) {
        let Some((sid, g)) = self.gemini.iter_mut().find(|(_, g)| g.key == key) else {
            return; // the inbox closed after the frame was read
        };
        match keep(frame, g.class, &mut g.rate, &g.spool, spool::MAX_PENDING) {
            Ok(()) => {
                lx_info!("peerbus.gemini.kept", "waiting for the session's next turn", sid = *sid, agent = g.record.agent.label(), peer_pid = peer_pid.unwrap_or(0));
            }
            Err((receipt, reason)) => {
                lx_info!("peerbus.refused", &reason, sid = *sid, peer_pid = peer_pid.unwrap_or(0));
                if let Some(to) = deliver::reply_address(frame) {
                    let address = format!("uds:{}", g.inbox.path.display());
                    deliver::send_receipt(&g.inbox.relay, &to, &address, receipt, frame.msg_id.as_deref(), &reason);
                }
            }
        }
    }

    /// `notify_when_idle` sent to a Gemini inbox.  The record does not
    /// offer it, so a sender that looked was told nothing would come;
    /// one that did not look gets the same nothing.
    pub(super) fn subscribe_gemini(&mut self, key: u64, from: String, msg_id: String) {
        match self.gemini.iter().find(|(_, g)| g.key == key).map(|(sid, g)| (*sid, g.record.agent)) {
            Some((sid, Agent::Kimi)) => self.subscribe_kimi(sid, from, msg_id),
            Some((sid, _)) => {
                lx_info!("peerbus.gemini.no_idle_notice", "idle subscription not held: nothing reports when a gemini session goes idle", sid = sid);
            }
            None => {}
        }
    }

    /// A notice for a subscription this Gemini session placed on
    /// another session: kept with its mail, in Claude Code's wording.
    pub(super) fn notice_gemini(&mut self, key: u64, from: &str, state: &str, finished_at: Option<i64>) {
        let Some((sid, g)) = self.gemini.iter().find(|(_, g)| g.key == key) else { return };
        let Some(text) = super::idle::notice_text(*sid, from, state, finished_at) else { return };
        match spool::append(&g.spool, &text, spool::MAX_PENDING) {
            Ok(true) => lx_info!("peerbus.gemini.notice", "idle notice waiting for the gemini session's next turn", sid = *sid),
            Ok(false) => lx_warn!("peerbus.gemini.notice_dropped", "the session's inbox is full", sid = *sid),
            Err(e) => lx_warn!("peerbus.gemini.notice_dropped", &e.to_string(), sid = *sid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::envelope::{self, Header};
    use marspot_peer::frame::Priority;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-gemini-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn frame(mode: Mode, body: &str) -> UserFrame {
        let h = Header { from: "uds:/tmp/cc-socks/9.sock".into(), from_name: Some("luna".into()), from_mode: Some(mode) };
        UserFrame { content: envelope::encode(&h, body), from: Some("uds:/tmp/cc-socks/9.sock".into()), priority: Priority::Next, msg_id: Some("m".into()) }
    }

    /// The record a Gemini pane is listed by: found by other sessions
    /// like a Codex one, and honest about what is not known of it.
    #[test]
    fn the_record_of_a_gemini_pane_reads_back_as_a_gemini_peer() {
        let d = dir();
        let me = std::process::id() as i32;
        let p = Peer {
            sid: 3,
            gemini: pidtree::ProcId { pid: 4242, start_us: 1 },
            session: "adc1c168-0d82-4875-9e77-9647b1774641".into(),
            cwd: "/work/gem".into(),
        };
        let at = Endpoint {
            relay_pid: me,
            proc_start: marspot_peer::proc::lstart(me).unwrap(),
            socket: "/tmp/cc-socks/4242-0a1b2c3d.sock".into(),
            relay: "/state/peerbus/7.send.sock".into(),
            spool: "/state/peerbus/gemini/adc1c168-0d82-4875-9e77-9647b1774641.jsonl".into(),
        };
        let record = record_for(&p, at, "gem".into(), Mode::Prompting, "/state", 5);
        registry::write(&d, &record, 6).unwrap();
        let live = registry::list_live(&d);
        let e = &live[0];
        assert!(e.ours && e.is_live());
        assert_eq!((e.agent, e.agent_pid), (Some(Agent::Gemini), Some(4242)));
        assert_eq!(e.name.as_deref(), Some("gem"));
        assert_eq!(e.session_id.as_deref(), Some(p.session.as_str()));
        assert_eq!(e.socket.as_deref(), Some("/tmp/cc-socks/4242-0a1b2c3d.sock"));
        assert_eq!(e.class, Some(Mode::Prompting));
        assert_eq!(e.status, None, "nothing reports a gemini session's status");
        assert!(!e.supports("notify_idle"), "and nothing reports it going idle");
        // the hook finds the session's mail through the record
        let spool = marspot_peer::gemini_hook::spool_of(&live, &p.session);
        assert_eq!(spool, Some(PathBuf::from("/state/peerbus/gemini/adc1c168-0d82-4875-9e77-9647b1774641.jsonl")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_a_yolo_command_line_is_the_bypass_class() {
        let a = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(class_of(&a("gemini")), Mode::Prompting);
        assert_eq!(class_of(&a("gemini -m flash --approval-mode auto_edit")), Mode::Prompting);
        assert_eq!(class_of(&a("gemini --approval-mode yolo")), Mode::Bypass);
        assert_eq!(class_of(&a("gemini --approval-mode=yolo -m pro")), Mode::Bypass);
        assert_eq!(class_of(&a("node /x/gemini.js --yolo")), Mode::Bypass);
        assert_eq!(class_of(&a("gemini -y")), Mode::Bypass);
        assert_eq!(class_of(&a("yolo")), Mode::Prompting, "argv[0] is not a flag");
        assert_eq!(class_of(&[]), Mode::Prompting);
    }

    #[test]
    fn an_admitted_message_waits_as_the_envelope_it_came_in() {
        let d = dir();
        let spool = spool::path_for(&d, "s1");
        let f = frame(Mode::Bypass, "do the thing");
        assert_eq!(keep(&f, Mode::Bypass, &mut Rate::default(), &spool, 2), Ok(()));
        assert_eq!(keep(&frame(Mode::Bypass, "and another"), Mode::Bypass, &mut Rate::default(), &spool, 2), Ok(()));
        // full: the receipt a full inbox channel gives
        let full = keep(&frame(Mode::Bypass, "one too many"), Mode::Bypass, &mut Rate::default(), &spool, 2);
        assert_eq!(full, Err((Receipt::Dropped(DropReason::QueueFull), "recipient inbox is full".to_string())));
        let waiting = spool::take(&spool);
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0], f.content, "what a Claude or Codex recipient is shown");
        assert!(waiting[0].starts_with("<cross-session-message from=\"uds:/tmp/cc-socks/9.sock\""), "{}", waiting[0]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_message_of_another_class_is_refused_and_not_kept() {
        let d = dir();
        let spool = spool::path_for(&d, "s1");
        let (receipt, reason) = keep(&frame(Mode::Bypass, "x"), Mode::Prompting, &mut Rate::default(), &spool, 2).unwrap_err();
        assert_eq!(receipt, Receipt::Refused);
        assert!(reason.contains("prompting"), "{reason}");
        assert!(!spool.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn mail_of_ended_sessions_is_swept_and_bound_sessions_keep_theirs() {
        let d = dir();
        let (kept, gone) = (spool::path_for(&d, "live"), spool::path_for(&d, "ended"));
        spool::append(&kept, "a", 4).unwrap();
        spool::append(&gone, "b", 4).unwrap();
        sweep(&d, &HashSet::from([kept.clone()]));
        assert!(kept.exists() && !gone.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
