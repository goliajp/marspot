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
