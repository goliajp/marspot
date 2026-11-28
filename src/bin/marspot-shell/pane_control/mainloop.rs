//! The main loop's half: send what the connections queued, refuse what
//! a pane's running script or a missing core rules out, and publish
//! the per-pane state the connections read.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use marspot::shell_proto::{MsgType, PaneWatch, WatchBody, WatchKind, encode_pane_watch};
use marspot_peer::control::{ErrorCode, Reject};

use super::{MainCmd, PaneView, Reply, hub};
use crate::ShellApp;
use crate::plugins::{PaneSession, PaneSessionHost};

type Snapshot = HashMap<u64, (marspot::pane_state::PaneStatus, std::time::Duration, bool)>;

pub(crate) struct MainSide {
    rx: Receiver<MainCmd>,
}

impl MainSide {
    pub(crate) fn start(wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> MainSide {
        MainSide { rx: super::start(wake) }
    }
}

/// How long after a wake an agent should try its input again: the
/// resume takes about two seconds, and the agent then loads.
const WAKE_RETRY_MS: u64 = 3_000;

/// The soonest a pane still loading after a wake is asked again.
const LOADING_RETRY_MIN_MS: u64 = 500;

/// Input for a pane whose agent sleeps (`parked`): the input wakes it,
/// as the user coming back to it would, and waits until it is up.
pub(crate) fn wake_for_input(s: &mut dyn PaneSession, host: &dyn PaneSessionHost) -> Option<Reject> {
    if !s.parked() {
        return None;
    }
    let woken = s.wake_for_input(host);
    marspot::lx_event!("PANE_CONTROL_WAKE", "input for a sleeping pane wakes it", session = host.shelld_session_id(),
        woken = woken as u32);
    if !woken {
        return Some(Reject::new(ErrorCode::PaneDormant,
            "the pane's agent was put to sleep idle, and what holds the pane does not wake it for input: it sleeps until the user comes back to the pane"));
    }
    let r = match s.waking_for() {
        Some(left) => Reject::new(ErrorCode::PaneDormant, format!(
            "the pane's agent was put to sleep idle and is waking for this request; it may take up to {} s to load, and until then pane_busy says when to try again",
            left.as_secs())),
        None => Reject::new(ErrorCode::PaneDormant, "the pane's agent was put to sleep idle and is waking for this request"),
    };
    Some(r.retry_after(WAKE_RETRY_MS))
}

/// The answer while a script runs on a pane; `waking` is what is left of
/// the time a wake may take, while the script is bringing the pane back.
pub(crate) fn busy(name: &str, waking: Option<std::time::Duration>) -> Reject {
    let Some(left) = waking else {
        return Reject::new(ErrorCode::PaneBusy, format!("a script is running on the pane ({name})"));
    };
    let ms = (left.as_millis() as u64).clamp(LOADING_RETRY_MIN_MS, WAKE_RETRY_MS);
    Reject::new(ErrorCode::PaneBusy, format!("the pane's agent is still waking ({name}), for {} s more at most", left.as_secs()))
        .retry_after(ms)
}

fn refuse(reply: Option<(u32, std::sync::mpsc::Sender<Reply>)>, r: Reject) {
    if let Some((id, tx)) = reply {
        let _ = tx.send(Reply::Refused(id, r));
    }
}

impl ShellApp {
    /// Send everything the control socket queued.
    pub(crate) fn drain_pane_control(&mut self) {
        let cmds: Vec<MainCmd> = self.pane_control.rx.try_iter().collect();
        for cmd in cmds {
            match cmd {
                MainCmd::Send { sid, msg, payload, guard_ops, reply } => {
                    if guard_ops && let Some(r) = self.ops_hold(sid) {
                        refuse(reply, r);
                        continue;
                    }
                    let sent = self.active.as_ref().filter(|c| c.hello_acked).is_some_and(|c| c.send(msg, &payload));
                    if !sent {
                        refuse(reply, Reject::new(ErrorCode::CoreRestarting, "the core is restarting").retry_after(1_000));
                    }
                }
                MainCmd::Resync => self.resync_watches(),
                MainCmd::PublishViews => {
                    let snapshot = self.pane_status.snapshot(std::time::Instant::now());
                    self.publish_views_now(&snapshot);
                }
            }
        }
    }

    /// Why a script on `sid` keeps input out, if one does.
    fn ops_hold(&mut self, sid: u64) -> Option<Reject> {
        let (dormant, waking) = self.with_pane_session(sid, |s, h| (wake_for_input(s, h), s.waking_for())).unzip();
        if let Some(r) = dormant.flatten() {
            return Some(r);
        }
        self.pty_ops.running(sid).map(|name| busy(name, waking.flatten()))
    }

    /// A core announced itself: whatever waits sessions still hold are
    /// cleared, then every wait of ours is registered again.
    fn resync_watches(&mut self) {
        let Some(h) = hub() else { return };
        let Some(conn) = self.active.as_ref().filter(|c| c.hello_acked) else { return };
        let obs = crate::observe::shared().latest();
        let clear = WatchBody { watch_id: 0, kind: WatchKind::Cancel, since_pos: 0, quiet_ms: 0, text: String::new() };
        let mut n = 0u32;
        for s in obs.live() {
            let watch = PaneWatch { req_id: h.links.req_id(), sid: s.entry.id, watch: clear.clone() };
            if let Ok(p) = encode_pane_watch(&watch) {
                conn.send(MsgType::PaneWatch, &p);
            }
        }
        for (sid, body) in h.links.watch_bodies() {
            if let Ok(p) = encode_pane_watch(&PaneWatch { req_id: h.links.req_id(), sid, watch: body }) {
                conn.send(MsgType::PaneWatch, &p);
                n += 1;
            }
        }
        marspot::lx_event!("PANE_CONTROL_RESYNC", "cleared the sessions' waits and registered ours again", waits = n);
    }

    /// Publish each pane's state for the connection threads, while any
    /// is connected.
    pub(crate) fn publish_pane_views(&self, snapshot: &Snapshot) {
        if hub().is_some_and(|h| h.wants_views()) {
            self.publish_views_now(snapshot);
        }
    }

    fn publish_views_now(&self, snapshot: &Snapshot) {
        let Some(h) = hub() else { return };
        let views = snapshot
            .iter()
            .map(|(sid, (status, _, _))| {
                let dormant = self.active_pane_sessions.get(sid).is_some_and(|a| a.session.parked());
                (*sid, PaneView { state: status.label(), busy: self.pty_ops.running(*sid), dormant })
            })
            .collect();
        h.publish_open_windows(self.windows.iter().map(|w| w.frame_index).collect());
        h.publish_panes(views);
    }

    pub(crate) fn forget_closed_for_control(open: &dyn Fn(u64) -> bool) {
        if let Some(h) = hub() {
            h.forget(open);
        }
    }
}
