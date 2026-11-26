//! The main loop's half: send what the connections queued, refuse what
//! a pane's running script or a missing core rules out, and publish
//! the per-pane state the connections read.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use marspot::shell_proto::{MsgType, PaneWatch, WatchBody, WatchKind, encode_pane_watch};
use marspot_peer::control::{ErrorCode, Reject};

use super::{MainCmd, PaneView, Reply, hub};
use crate::ShellApp;

type Snapshot = HashMap<u64, (marspot::pane_state::PaneStatus, std::time::Duration, bool)>;

pub(crate) struct MainSide {
    rx: Receiver<MainCmd>,
}

impl MainSide {
    pub(crate) fn start(wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> MainSide {
        MainSide { rx: super::start(wake) }
    }
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
    fn ops_hold(&self, sid: u64) -> Option<Reject> {
        if self.active_pane_sessions.get(&sid).is_some_and(|a| a.session.parked()) {
            return Some(Reject::new(ErrorCode::PaneDormant, "the pane's agent was put to sleep idle and waits for the user to come back"));
        }
        self.pty_ops
            .running(sid)
            .map(|name| Reject::new(ErrorCode::PaneBusy, format!("a script is running on the pane ({name})")))
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
