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
//!
//! ## Lifecycle reconcile
//!
//! After each `build_view` + `layout` pass,call
//! `reconcile(&laid_out)` —— framework walks the tree,collects
//! every `ViewId` reachable,then `HOST_STATE.remove()` any id no
//! longer in the tree(on_disappear semantics).  Forgotten state
//! prevents stale offsets from haunting recreated views.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use super::layout::LaidOut;
use super::types::ViewId;
use super::view::View;

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
        if !self.inner.contains_key(&key) {
            self.inner.insert(key, Box::new(T::default()));
        }
        self.inner.get_mut(&key).unwrap().downcast_mut::<T>().unwrap()
    }

    pub fn remove<T: 'static>(&mut self, id: ViewId) {
        self.inner.remove(&(id, TypeId::of::<T>()));
    }

    /// Drop every slot whose id isn't in `live`.  Called by
    /// `reconcile()`.
    pub fn retain_ids(&mut self, live: &HashSet<ViewId>) {
        self.inner.retain(|(id, _t), _v| live.contains(id));
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

/// Lifecycle reconcile — walk the laid-out tree,collect every
/// ViewId in use this frame,drop any HostState slot whose id
/// disappeared(on_disappear semantics).
///
/// Currently no on_appear hook(immediate-mode views are pure data
/// constructors,nothing to invoke at first build).When stateful
/// view types add init hooks(eg TextField loading saved cursor),
/// they'll plug in here.
pub fn reconcile(laid: &LaidOut) {
    let mut live: HashSet<ViewId> = HashSet::new();
    collect_ids(laid, &mut live);
    // Also collect implicit ids — ScrollView etc. carry their own id
    // in the `View` enum directly,not via `Modifier::Id`.
    with_host_state_mut(|s| s.retain_ids(&live));
}

fn collect_ids(laid: &LaidOut, out: &mut HashSet<ViewId>) {
    if let Some(id) = laid.deco.id {
        out.insert(id);
    }
    if let View::ScrollView { id, .. } = &laid.view {
        out.insert(*id);
    }
    for ch in laid.children.iter() {
        collect_ids(ch, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct FakeState {
        n: i32,
    }
    impl Default for FakeState { fn default() -> Self { Self { n: 0 } } }

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

    #[test]
    fn retain_drops_missing_ids() {
        let mut s = HostState::new();
        s.insert(ViewId(1), FakeState { n: 1 });
        s.insert(ViewId(2), FakeState { n: 2 });
        let mut live = HashSet::new();
        live.insert(ViewId(2));
        s.retain_ids(&live);
        assert_eq!(s.get::<FakeState>(ViewId(1)), None);
        assert_eq!(s.get::<FakeState>(ViewId(2)), Some(&FakeState { n: 2 }));
    }
}
