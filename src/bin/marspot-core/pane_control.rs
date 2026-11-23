//! Typing into a pane and watching it, for L1: each request goes to
//! the pane's session unchanged, and what the session reports goes
//! back to L1 unchanged.
//!
//! Every request is answered.  When the core cannot hand one to the
//! session it answers it itself with a `PaneDone`: `not_attached`
//! without a live session connection, `old_session` when the session's
//! image never said it reads these frames (see `pane::control`).
//! Nothing here asks for a redraw; the only cost is the wake that
//! carried the frame in.

use super::pane_reports::Reports;
use super::*;
use marspot::pane::ControlReach;
use marspot::shell_proto::{
    CAP_HELPER_PANEL, CAP_SHADOWS, CAP_TYPE_AND_WATCH, DoneCode, PaneDone, decode_pane_type, decode_pane_watch,
    encode_pane_control_caps, encode_pane_done,
};
use marspot_term::frame_writer::FrameWriter;

/// What became of one request from L1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Queued for the session.
    Forwarded,
    /// Answered here with this code.
    Answered(DoneCode),
    /// Too short to carry a request id: nobody to answer.
    Unanswerable,
}

#[derive(Default)]
pub(super) struct PaneControl {
    pub(super) reports: Reports,
    caps_sent: bool,
    pub(super) shadows: super::shadows::Shadows,
    /// Dev seam only (`MARSPOT_DEV_PANE_CONTROL`): when to play one
    /// `PaneType` and one `PaneWatch` for the first session pane.
    dev_at: Option<Instant>,
}

impl PaneControl {
    pub(super) fn from_env() -> Self {
        Self {
            dev_at: std::env::var("MARSPOT_DEV_PANE_CONTROL")
                .is_ok()
                .then(|| Instant::now() + Duration::from_secs(3)),
            shadows: super::shadows::Shadows::from_env(),
            ..Self::default()
        }
    }

    /// After the `HelloAck`, once: what this core does with these frames.
    pub(super) fn caps_after_hello(&mut self) {
        if std::mem::replace(&mut self.caps_sent, true) {
            return;
        }
        let caps = CAP_TYPE_AND_WATCH | CAP_SHADOWS | CAP_HELPER_PANEL;
        self.reports.push(Frame::new(
            MsgType::PaneControlCaps,
            encode_pane_control_caps(caps),
        ));
        lx_event!(
            "PANE_CONTROL_CAPS",
            "told L1 what this core does with pane control",
            caps = caps
        );
    }

    /// The loop's wait for held frames and for the dev seam.
    pub(super) fn wait(&self) -> Option<Duration> {
        let dev = self.dev_at.map(|t| {
            t.saturating_duration_since(Instant::now())
                .max(Duration::from_millis(250))
        });
        [self.reports.retry_after(), dev, self.shadows.wait()]
            .into_iter()
            .flatten()
            .min()
    }

    pub(super) fn flush(&mut self, w: &FrameWriter) {
        self.reports.flush(w);
    }
}

fn code_name(code: DoneCode) -> &'static str {
    match code {
        DoneCode::Ok => "ok",
        DoneCode::NotAttached => "not_attached",
        DoneCode::NoSuchSession => "no_such_session",
        DoneCode::WatchTableFull => "watch_table_full",
        DoneCode::OldSession => "old_session",
        DoneCode::Other | DoneCode::Unknown(_) => "other",
    }
}

impl CoreApp {
    /// L1 asked to type into, or watch, session `sid`.
    pub(super) fn route_pane_control(&mut self, f: Frame) -> Route {
        let Some(req_id) = f.payload.first_chunk::<4>().map(|b| u32::from_le_bytes(*b)) else {
            lx_warn!(
                "core.pane_control.unanswerable",
                "a pane-control request too short for its request id",
                msg_type = format!("{:?}", f.msg_type),
                len = f.payload.len()
            );
            return Route::Unanswerable;
        };
        if super::shadows::is_shadow_request(f.msg_type) {
            return self.route_shadow_request(f, req_id);
        }
        // a request cut short after its id is still answered, as malformed
        let sid = f
            .payload
            .get(4..12)
            .and_then(|b| b.first_chunk::<8>())
            .map_or(0, |b| u64::from_le_bytes(*b));
        let decodes = match f.msg_type {
            MsgType::PaneType => decode_pane_type(&f.payload).is_ok(),
            MsgType::PaneWatch => decode_pane_watch(&f.payload).is_ok(),
            _ => false,
        };
        if !decodes {
            return self.answer(
                f.msg_type,
                req_id,
                sid,
                DoneCode::Other,
                "malformed request",
            );
        }
        let reconnecting = self.reconnecting.contains(&sid);
        let msg_type = f.msg_type;
        let reach = match self.pane_by_sid_mut(sid) {
            None => ControlReach::NotAttached,
            Some(_) if reconnecting => ControlReach::NotAttached,
            Some(p) => match p.session().pane_control_reach() {
                ControlReach::Reads => {
                    if !p.session().send_pane_control(f) {
                        return self.answer(
                            msg_type,
                            req_id,
                            sid,
                            DoneCode::Other,
                            "session queue full",
                        );
                    }
                    lx_event!(
                        "PANE_CONTROL_FORWARDED",
                        "a pane-control request went to its session",
                        msg_type = format!("{msg_type:?}"),
                        req_id = req_id,
                        session = sid
                    );
                    return Route::Forwarded;
                }
                other => other,
            },
        };
        let code = match reach {
            ControlReach::OldSession => DoneCode::OldSession,
            _ => DoneCode::NotAttached,
        };
        self.answer(msg_type, req_id, sid, code, "")
    }

    pub(super) fn answer(
        &mut self,
        msg_type: MsgType,
        req_id: u32,
        sid: u64,
        code: DoneCode,
        text: &str,
    ) -> Route {
        lx_event!(
            "PANE_CONTROL_REPLY",
            "the core answered a pane-control request itself",
            msg_type = format!("{msg_type:?}"),
            req_id = req_id,
            session = sid,
            code = code_name(code)
        );
        if super::shadow_place::is_own(req_id) {
            return Route::Answered(code);
        }
        let done = PaneDone {
            req_id,
            code,
            text: text.to_string(),
        };
        match encode_pane_done(&done) {
            Ok(payload) => self
                .pane_control
                .reports
                .push(Frame::new(MsgType::PaneDone, payload)),
            Err(e) => lx_error!(
                "core.pane_control.encode_failed",
                &format!("{e}"),
                req_id = req_id
            ),
        }
        Route::Answered(code)
    }

    /// Session `sid` reported on a request L1 sent through: on to L1,
    /// unless it answers one of the core's own.
    pub(super) fn report_to_shell(&mut self, sid: u64, f: Frame) {
        if f.msg_type == MsgType::PaneDone && self.session_done(sid, &f) {
            return;
        }
        self.pane_control.reports.push(f);
    }

    /// Once per loop turn: moves whose session did not answer in time,
    /// and the dev seams.
    pub(super) fn pane_control_tick(&mut self) {
        self.expire_places(Instant::now());
        self.helper_panel_tick();
        self.dev_drive_pane_control();
        self.dev_drive_shadows();
    }

    /// Dev seam (`MARSPOT_DEV_PANE_CONTROL`) — see `PaneControl::dev_at`.
    /// The input is empty and the watch request clears no watch L1
    /// made, so the pane sees nothing; the answers go to L1 as any
    /// other would.
    pub(super) fn dev_drive_pane_control(&mut self) {
        match self.pane_control.dev_at {
            Some(t) if Instant::now() >= t => {}
            _ => return,
        }
        let Some(sid) = self.panes_mut().find_map(|p| p.session().l3_session_id()) else {
            return;
        };
        self.pane_control.dev_at = None;
        let typed = marspot::shell_proto::PaneType {
            req_id: u32::MAX - 1,
            sid,
            paste: false,
            enter: false,
            only_at_shell: false,
            bytes: Vec::new(),
            watch: None,
        };
        let watch = marspot::shell_proto::PaneWatch {
            req_id: u32::MAX,
            sid,
            watch: marspot::shell_proto::WatchBody {
                watch_id: u32::MAX,
                kind: marspot::shell_proto::WatchKind::Cancel,
                since_pos: 0,
                quiet_ms: 0,
                text: String::new(),
            },
        };
        let frames = [
            marspot::shell_proto::encode_pane_type(&typed)
                .map(|p| Frame::new(MsgType::PaneType, p)),
            marspot::shell_proto::encode_pane_watch(&watch)
                .map(|p| Frame::new(MsgType::PaneWatch, p)),
        ];
        for f in frames.into_iter().flatten() {
            let route = self.route_pane_control(f);
            lx_event!(
                "DEV_PANE_CONTROL",
                "played a pane-control request (dev seam)",
                session = sid,
                route = format!("{route:?}")
            );
        }
    }
}

#[cfg(test)]
#[path = "pane_control_tests.rs"]
mod tests;
