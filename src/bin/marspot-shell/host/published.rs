//! A value one thread writes and others read as a snapshot.
//!
//! A reader takes an `Arc` of the value last published and drops the
//! lock at once: it never waits on a writer's work, only on the swap of
//! one pointer.  A writer builds the new value without the lock, and
//! content equal to what is there is not published again, so a reader
//! that remembers the `Arc` it last saw can tell "nothing changed" by
//! pointer alone.  Writers are serialised among themselves by a second
//! lock that readers never take, so a read-modify-write (`update`) sees
//! no other writer's change in between.
//!
//! A panic never reaches another thread through here: both locks are
//! taken back from a poisoned state.  The snapshot lock guards nothing
//! but a pointer swap, which cannot leave it half done; a writer that
//! panics in `update` loses only its own copy, and the value published
//! before stays.

use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) struct Published<T> {
    current: Mutex<Option<Arc<T>>>,
    writer: Mutex<()>,
}

fn take<G>(r: std::sync::LockResult<G>) -> G {
    r.unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<T> Published<T> {
    /// Nothing published yet: `get` is None until the first `publish`.
    pub(crate) const fn new() -> Self {
        Self { current: Mutex::new(None), writer: Mutex::new(()) }
    }

    /// Published from the start with `value`.
    pub(crate) fn with(value: T) -> Self {
        Self { current: Mutex::new(Some(Arc::new(value))), writer: Mutex::new(()) }
    }

    /// The value last published; None before the first.
    pub(crate) fn get(&self) -> Option<Arc<T>> {
        take(self.current.lock()).clone()
    }

    fn write_lock(&self) -> MutexGuard<'_, ()> {
        take(self.writer.lock())
    }

    /// Swap in `next`; the old value is dropped after the lock is let go.
    fn swap(&self, next: T) {
        let old = take(self.current.lock()).replace(Arc::new(next));
        drop(old);
    }
}

impl<T: PartialEq> Published<T> {
    /// Make `value` what readers get, unless it equals what they get
    /// now.  True when it was published.
    pub(crate) fn publish(&self, value: T) -> bool {
        let _w = self.write_lock();
        if self.get().is_some_and(|now| *now == value) {
            return false;
        }
        self.swap(value);
        true
    }
}

impl<T: Clone + PartialEq + Default> Published<T> {
    /// Change a copy of the current value (the default before the
    /// first publish) with `f`, and publish the copy if `f` changed it.
    /// No other writer runs meanwhile; readers go on getting the value
    /// from before until the copy is swapped in.
    pub(crate) fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _w = self.write_lock();
        let now = self.get();
        let mut next = now.as_deref().cloned().unwrap_or_default();
        let out = f(&mut next);
        if now.as_deref() != Some(&next) {
            self.swap(next);
        }
        out
    }
}

#[cfg(test)]
#[path = "published_tests.rs"]
mod tests;
