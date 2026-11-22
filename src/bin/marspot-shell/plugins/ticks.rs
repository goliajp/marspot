//! When each plugin is ticked.
//!
//! A plugin's tick runs at most once per its `tick_interval_ms`, on a
//! pass of the supervisor.  The supervisor no longer passes once a
//! second while panes are awake: it passes when something happens and
//! when something is due, and a plugin says which of its ticks are due
//! ([`TickDue`]).  A plugin with nothing to watch, no program of its
//! own in any pane and nothing in flight, needs no tick until something
//! changes; the observer knocks when the programs in the panes change,
//! and the tick that knock brings finds its program.

use std::time::{Duration, Instant};

use super::{PluginHost, PluginRegistry, Slot, dispatch, run_hook_void};

/// What a plugin's next tick is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickDue {
    /// Every `tick_interval_ms`, as every plugin was ticked: it watches
    /// something that changes with no event to say so.
    Cadence,
    /// Not before this moment: the one thing it still watches on a
    /// clock comes due then.
    At(Instant),
    /// Not until something it would act on has changed.
    Event,
}

impl Slot {
    fn interval(&self) -> Duration {
        Duration::from_millis((self.metadata.tick_interval_ms as u64).max(100))
    }

    /// When this slot is next to be ticked; None when no tick is wanted
    /// until something changes.
    fn due(&self) -> Option<Instant> {
        if !self.enabled || self.metadata.tick_interval_ms == 0 {
            return None;
        }
        let cadence = self.last_tick + self.interval();
        match self.plugin.tick_due() {
            TickDue::Cadence => Some(cadence),
            TickDue::At(t) => Some(t.max(cadence)),
            TickDue::Event => None,
        }
    }
}

impl PluginRegistry {
    pub fn tick_all(&mut self, host: &dyn PluginHost) {
        if !self.started {
            return;
        }
        let now = Instant::now();
        for slot in self.slots.iter_mut() {
            if slot.due().is_none_or(|t| now < t) {
                continue;
            }
            slot.last_tick = now;
            dispatch::with(host, slot.metadata.name, || {
                run_hook_void(slot, "tick", |p| p.tick(host));
            });
        }
    }

    /// The soonest tick any plugin wants; None when none wants one until
    /// something changes.
    pub fn next_due(&self) -> Option<Instant> {
        if !self.started {
            return None;
        }
        self.slots.iter().filter_map(Slot::due).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::host::ShellPluginHost;
    use crate::plugins::{PLUGIN_API_VERSION, Plugin, PluginMetadata};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Counted {
        due: TickDue,
        ticks: Arc<AtomicU32>,
    }

    impl Plugin for Counted {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata { name: "counted", api_version: PLUGIN_API_VERSION, tick_interval_ms: 2000 }
        }
        fn tick(&mut self, _host: &dyn PluginHost) {
            self.ticks.fetch_add(1, Ordering::Relaxed);
        }
        fn tick_due(&self) -> TickDue {
            self.due
        }
    }

    fn registry(due: TickDue) -> (PluginRegistry, Arc<AtomicU32>) {
        let ticks = Arc::new(AtomicU32::new(0));
        let mut r = PluginRegistry::new();
        r.register(Box::new(Counted { due, ticks: Arc::clone(&ticks) }));
        r.started = true;
        (r, ticks)
    }

    #[test]
    fn a_plugin_on_its_cadence_is_due_an_interval_after_its_last_tick() {
        let (mut r, _) = registry(TickDue::Cadence);
        let at = Instant::now();
        r.slots[0].last_tick = at;
        assert_eq!(r.next_due(), Some(at + Duration::from_secs(2)));
    }

    #[test]
    fn a_plugin_waiting_for_an_event_is_never_due_and_is_not_ticked() {
        let (mut r, ticks) = registry(TickDue::Event);
        assert_eq!(r.next_due(), None);
        r.tick_all(&ShellPluginHost::new());
        assert_eq!(ticks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_plugin_due_at_a_moment_is_not_ticked_before_it() {
        let later = Instant::now() + Duration::from_secs(60);
        let (mut r, ticks) = registry(TickDue::At(later));
        assert_eq!(r.next_due(), Some(later));
        r.tick_all(&ShellPluginHost::new());
        assert_eq!(ticks.load(Ordering::Relaxed), 0);
        r.slots[0].plugin = Box::new(Counted { due: TickDue::Cadence, ticks: Arc::clone(&ticks) });
        r.slots[0].last_tick = Instant::now() - Duration::from_secs(3);
        r.tick_all(&ShellPluginHost::new());
        assert_eq!(ticks.load(Ordering::Relaxed), 1, "on its cadence again, and its interval has passed");
    }
}
