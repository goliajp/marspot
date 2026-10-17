//! HostState — generic per-`ViewId` state map for stateful views.
//!
//! Replaces the per-view-type thread_local pattern(`scroll.rs`
//! has its own `SCROLL_STATES` — now backed by this).  Any stateful
//! view(`ScrollView` / `TextField` / `Toggle` / `Picker` / future)
//! stores its state in `HOST_STATE` keyed by its `ViewId`,typed
//! via `Any`.
//!
//! ## Why thread_local for v1
//!
//! Single NSWindow / single render thread.  When marspot grows to
//! multi-window we'll promote this to per-app or per-window state.
//! API stays the same; impl changes.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;

use super::types::ViewId;

/// Per-ViewId state store keyed by(`ViewId`, `TypeId`).Two views
/// can share a ViewId for different state types(eg a scrollview
/// AND a textfield both with id=42 would each get their own slot).
#[derive(Default)]
pub struct HostState {
    inner: HashMap<(ViewId, TypeId), Box<dyn Any>>,
}

impl HostState {
    pub fn new() -> Self { Self::default() }

    pub fn get<T: 'static>(&self, id: ViewId) -> Option<&T> {
        self.inner.get(&(id, TypeId::of::<T>()))
            .and_then(|b| b.downcast_ref::<T>())
    }

    pub fn get_mut<T: 'static>(&mut self, id: ViewId) -> Option<&mut T> {
        self.inner.get_mut(&(id, TypeId::of::<T>()))
            .and_then(|b| b.downcast_mut::<T>())
    }

    pub fn insert<T: 'static>(&mut self, id: ViewId, v: T) {
        self.inner.insert((id, TypeId::of::<T>()), Box::new(v));
    }

    pub fn entry_or_default<T: 'static + Default>(&mut self, id: ViewId) -> &mut T {
        let key = (id, TypeId::of::<T>());
        self.inner.entry(key).or_insert_with(|| Box::new(T::default()));
        self.inner.get_mut(&key).unwrap().downcast_mut::<T>().unwrap()
    }

    pub fn remove<T: 'static>(&mut self, id: ViewId) {
        self.inner.remove(&(id, TypeId::of::<T>()));
    }

    pub fn len(&self) -> usize { self.inner.len() }
    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
}

thread_local! {
    pub static HOST_STATE: RefCell<HostState> = RefCell::new(HostState::new());
}

/// Read-only access(via closure to avoid borrow issues).
pub fn with_host_state<R>(f: impl FnOnce(&HostState) -> R) -> R {
    HOST_STATE.with(|s| f(&s.borrow()))
}

/// Read-write access.
pub fn with_host_state_mut<R>(f: impl FnOnce(&mut HostState) -> R) -> R {
    HOST_STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Anim registry — active animations driven by the host frame
/// scheduler.  Each animation is keyed by a unique `AnimKey`;
/// `tick(now)` advances all of them by elapsed time.  Host queries
/// `any_active()` after build/tick to decide whether to schedule
/// the next vsync redraw.
pub struct AnimRegistry {
    inner: HashMap<u64, AnimSlot>,
    last_tick: Option<std::time::Instant>,
    next_key: u64,
}
struct AnimSlot {
    elapsed_ms: f64,
    duration_ms: f64,
    /// `true` when elapsed >= duration.
    finished: bool,
}

impl Default for AnimRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimRegistry {
    pub fn new() -> Self {
        Self { inner: HashMap::new(), last_tick: None, next_key: 1 }
    }
    /// Register a new animation, return its key.  Caller can query
    /// progress via `get(key)` later.
    pub fn start(&mut self, duration_ms: f64) -> u64 {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1);
        self.inner.insert(key, AnimSlot { elapsed_ms: 0.0, duration_ms, finished: false });
        key
    }
    /// Advance all anims by the wall-clock since the last tick.
    pub fn tick(&mut self, now: std::time::Instant) {
        let dt_ms = match self.last_tick {
            Some(prev) => (now - prev).as_secs_f64() * 1000.0,
            None       => 0.0,
        };
        self.last_tick = Some(now);
        for slot in self.inner.values_mut() {
            if slot.finished { continue; }
            slot.elapsed_ms += dt_ms;
            if slot.elapsed_ms >= slot.duration_ms {
                slot.elapsed_ms = slot.duration_ms;
                slot.finished = true;
            }
        }
    }
    /// `t ∈ [0, 1]` for the named anim.  Returns `None` if unknown.
    pub fn progress(&self, key: u64) -> Option<f64> {
        self.inner.get(&key).map(|s| {
            if s.duration_ms <= 0.0 { 1.0 }
            else { (s.elapsed_ms / s.duration_ms).clamp(0.0, 1.0) }
        })
    }
    /// Has any anim not yet finished?  Host uses this to schedule
    /// the next vsync (~16ms) — when false, scheduler can park,
    /// preserving `idle CPU = 0`.
    pub fn any_active(&self) -> bool {
        self.inner.values().any(|s| !s.finished)
    }
    /// Drop finished anims.  Call after `tick()` to keep map small.
    pub fn gc(&mut self) {
        self.inner.retain(|_, s| !s.finished);
    }
}

thread_local! {
    pub static ANIM_REGISTRY: std::cell::RefCell<AnimRegistry> =
        std::cell::RefCell::new(AnimRegistry::new());
}

pub fn anim_tick(now: std::time::Instant) {
    ANIM_REGISTRY.with(|r| r.borrow_mut().tick(now));
}
pub fn anim_any_active() -> bool {
    ANIM_REGISTRY.with(|r| r.borrow().any_active())
}
pub fn anim_gc() {
    ANIM_REGISTRY.with(|r| r.borrow_mut().gc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    #[derive(Default)]
    struct FakeState {
        n: i32,
    }
    

    #[test]
    fn insert_then_get() {
        let mut s = HostState::new();
        s.insert(ViewId(1), FakeState { n: 42 });
        assert_eq!(s.get::<FakeState>(ViewId(1)), Some(&FakeState { n: 42 }));
        assert_eq!(s.get::<FakeState>(ViewId(2)), None);
    }

    #[test]
    fn entry_or_default_initialises() {
        let mut s = HostState::new();
        let v: &mut FakeState = s.entry_or_default(ViewId(5));
        assert_eq!(v.n, 0);
        v.n = 99;
        assert_eq!(s.get::<FakeState>(ViewId(5)), Some(&FakeState { n: 99 }));
    }

    #[test]
    fn type_id_segregates_slots_on_same_view_id() {
        #[derive(Debug, PartialEq, Default)]
        struct OtherState { s: String }
        let mut s = HostState::new();
        s.insert(ViewId(1), FakeState { n: 7 });
        s.insert(ViewId(1), OtherState { s: "hi".into() });
        assert_eq!(s.get::<FakeState>(ViewId(1)), Some(&FakeState { n: 7 }));
        assert_eq!(s.get::<OtherState>(ViewId(1)), Some(&OtherState { s: "hi".into() }));
    }
}
