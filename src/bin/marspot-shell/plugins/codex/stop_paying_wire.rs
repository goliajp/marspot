//! The credits rule over the plugin's panes: what each needs of a
//! target, whether it may switch now, and starting the switches ordered.

use std::collections::HashMap;

use super::CodexPlugin;
use super::credits::Credit;
use super::pane::PaneCodex;
use super::stop_paying::{COOL_DOWN, Mover, need, say};
use super::stop_paying_screen::Looks;
use super::switch::{Switches, movable_argv};
use crate::plugins::PluginHost;
use crate::plugins::allocation::speed::default_rate;
use crate::plugins::allocation::Class;

impl CodexPlugin {
    /// The credits rule over every codex pane, with what the allocation
    /// pass read of each account and pane.
    pub(super) fn stop_paying(&mut self, host: &dyn PluginHost, credit: Option<Credit>) {
        let Some(Credit { now, accts, rates }) = credit else { return };
        let mode = marspot::settings::get().sched.credits;
        let stay_h = crate::plugins::claudecode::allocation_level2::limits::current().dwell_h;
        let mut log = Vec::new();
        self.paying.follow_afters(&accts, now, &mut log);
        self.paying.forget(|sid| self.panes.contains_key(&sid), now);
        let mut sids: Vec<(u64, u8)> = self.panes.iter().filter_map(|(sid, p)| Some((*sid, p.profile?))).collect();
        sids.sort_unstable();
        let mut orders = Vec::new();
        let looks = self.paying.looks;
        for (sid, from) in sids {
            // a pane the allocation has not measured is taken at its default
            let rate = rates.get(&sid).copied().unwrap_or_else(|| {
                let (f, n, _) = default_rate(Class::Plain);
                f + n
            });
            let m = Mover { sid, from, need: need(stay_h, rate) };
            let (switches, panes) = (&self.switches, &self.panes);
            let ready = || auto_ready(host, switches, &looks, panes, sid);
            orders.extend(self.paying.decide(mode, &accts, now, m, ready, &mut log));
        }
        for o in orders {
            let dir = super::profile_dirs().into_iter().find(|(n, _)| *n == o.to).map(|(_, d)| d);
            let codex = self.panes.get(&o.sid).map(|p| p.codex);
            let begun = match (dir, codex) {
                (Some(dir), Some(codex)) => self.begin_switch(o.sid, codex, o.to, dir, true),
                _ => false,
            };
            if !begun {
                self.paying.undo(o.sid, &mut log, "cannot_begin");
            }
        }
        say(host, log);
    }
}

/// Why pane `sid` may not be switched now by the rule, if it may not:
/// the menu switch's checks, an empty input box, and the cool-down.
fn auto_ready(host: &dyn PluginHost, switches: &Switches, looks: &Looks, panes: &HashMap<u64, PaneCodex>, sid: u64) -> Result<(), &'static str> {
    let pane = panes.get(&sid).ok_or("gone")?;
    if switches.moving(sid) {
        return Err("switching");
    }
    if switches.began_within(sid, COOL_DOWN) {
        return Err("cool_down");
    }
    if !pane.argv.as_deref().is_some_and(movable_argv) {
        return Err("custom_command");
    }
    let view = crate::plugins::peerbus::threads().get(&sid).filter(|v| v.codex == pane.codex).map(|v| v.idle);
    match view {
        None => return Err("thread_unknown"),
        Some(false) => return Err("busy"),
        Some(true) => {}
    }
    if !looks.settled(host, sid) {
        return Err("terminal_busy");
    }
    if !looks.input_empty(sid) {
        return Err("input_not_empty");
    }
    Ok(())
}
