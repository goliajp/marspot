//! Warning a busy pane that its account is about to refuse it.

use super::*;

impl ClaudecodePlugin {
    /// Tell a busy pane, once each time its account runs low, that the
    /// account is one step from refusing it -- while it can still answer -- when there is
    /// somewhere else for it to go.
    ///
    /// A resting pane is not told: moving it ends nothing, and the
    /// message would start a turn of its own.
    pub(super) fn heads_up_if_due(
        &mut self,
        host: &dyn PluginHost,
        sid: u64,
        rooms: &[(u8, quota::Room)],
        meta: &BindMeta,
        result: &ScanResult,
        // what the pane is judged as: a pane refused Fable as Fable
        model: Option<&str>,
    ) {
        let Some((_, room)) = rooms.iter().find(|(p, _)| *p == meta.profile_num) else { return };
        let week = room.reset_7d();
        // the account came back, at a new five hours or a new week: the
        // next time it nears out the pane is told again
        if self.heads_up.get(&sid).is_some_and(|h| h.renewed_on(meta.profile_num, room, model)) {
            self.heads_up.remove(&sid);
        }
        if self.heads_up.get(&sid).is_some_and(|h| h.is_for(meta.profile_num, week))
            || !heads_up::nearly_out(room, model)
            || !rooms.iter().any(|(p, r)| *p != meta.profile_num && !r.refused_for(model) && *r != quota::Room::Unknown)
        {
            return;
        }
        let resting = host.pane_status(sid).ok().flatten().is_some_and(|v| {
            v.quiescent && matches!(v.status, marspot::pane_state::PaneStatus::AwaitingUser)
        });
        let tool_executing =
            matches!(result.new_activity.get(&sid), Some(CcActivity::ToolPending { executing: true }));
        let work_running = result.new_vetoes.get(&sid).is_none_or(|v| v.0);
        if resting && !tool_executing && !work_running {
            return;
        }
        let delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        host.log(LogLevel::Info, "quota.heads_up", &format!("sid={sid} P{} is nearly out; telling it", meta.profile_num));
        heads_up::send(sid, meta.claude.pid, meta.profile_num, Arc::clone(&delivered));
        self.heads_up.insert(sid, heads_up::Sent { profile: meta.profile_num, week, delivered });
    }
}
