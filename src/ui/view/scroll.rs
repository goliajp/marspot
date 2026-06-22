//! `ScrollView` — clip + content-offset viewport.
//!
//! v1 vertical-only(主要是 dev panel / sidebar / process panel /
//! search overlay 都垂直滚).Horizontal / Both 留 v2 follow-up.
//!
//! ## Statefulness
//!
//! Scroll offset is per-`ViewId` state held in a process-thread-local
//! map(`SCROLL_STATES`).这是 v3 "Identity / State map" 的最简实
//! 现 —— 等更多 stateful view(TextField / Toggle / Picker)接进
//! 来后再统一升级到 host-owned `HashMap<ViewId, Box<dyn Any>>` +
//! lifecycle reconcile.
//!
//! Why thread_local for v1:
//! - 一次只有一个 NSWindow / 一个 render thread carries this
//! - 没有 cross-thread sharing,没有 send/sync 复杂度
//! - layout 跟 paint 都可以读取,event 路由可以写入
//! - 测试时一个测试一个 thread,天然隔离
//!
//! ## Paint clip
//!
//! Canvas 当前没有 clip primitive;v1 走"culling"近似:paint pass
//! 只 emit 跟 viewport intersect 的 child.partially-clipped child
//! 整个 paint,可能小幅 overflow 边缘 — 对 dev panel 来说够用,
//! 真 clip rect 走 v3 follow-up(需要 Metal scissor / stencil)。

use std::cell::RefCell;
use std::collections::HashMap;

use super::types::ViewId;

/// Per-ViewId scroll offset state.  `offset_y` is current scroll
/// position in physical pixels (0 = content top at viewport top).
/// `content_h` is last-measured content size for clamping.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    pub offset_y: f64,
    pub content_h: f64,
    pub viewport_h: f64,
}

impl ScrollState {
    /// Maximum reachable offset_y given current content / viewport.
    /// Negative when content fits entirely(no scroll needed).
    pub fn max_offset(&self) -> f64 {
        (self.content_h - self.viewport_h).max(0.0)
    }

    pub fn clamp_offset(&mut self) {
        self.offset_y = self.offset_y.max(0.0).min(self.max_offset());
    }
}

thread_local! {
    static SCROLL_STATES: RefCell<HashMap<ViewId, ScrollState>> =
        RefCell::new(HashMap::new());
}

/// Read current scroll state for `id`.  Returns default(0 offset,
/// 0 content_h, 0 viewport_h)if id unknown.
pub fn scroll_state(id: ViewId) -> ScrollState {
    SCROLL_STATES.with(|m| m.borrow().get(&id).copied().unwrap_or_default())
}

/// Write scroll state for `id`.  Used by layout(updating content_h
/// / viewport_h)and by event-loop scroll wheel handlers(updating
/// offset_y).
pub fn set_scroll_state(id: ViewId, s: ScrollState) {
    SCROLL_STATES.with(|m| m.borrow_mut().insert(id, s));
}

/// Mutate scroll state in place.  Returns the updated state for
/// convenience(callers often want to read post-mutation).
pub fn with_scroll_state(id: ViewId, f: impl FnOnce(&mut ScrollState)) -> ScrollState {
    SCROLL_STATES.with(|m| {
        let mut g = m.borrow_mut();
        let s = g.entry(id).or_default();
        f(s);
        *s
    })
}

/// Apply a scroll-wheel delta(in physical pixels)to `id`.
/// Positive `delta_y` = scroll content up(reveal content below).
/// Clamps result against current content_h / viewport_h.
pub fn apply_scroll_delta(id: ViewId, delta_y: f64) -> ScrollState {
    with_scroll_state(id, |s| {
        s.offset_y += delta_y;
        s.clamp_offset();
    })
}

/// Forget state for `id` — called by lifecycle reconcile when a
/// ScrollView no longer exists in the View tree.
pub fn forget(id: ViewId) {
    SCROLL_STATES.with(|m| { m.borrow_mut().remove(&id); });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_pushes_offset_into_range() {
        let mut s = ScrollState { offset_y: 1000.0, content_h: 500.0, viewport_h: 200.0 };
        s.clamp_offset();
        // max = 500 - 200 = 300
        assert_eq!(s.offset_y, 300.0);
    }

    #[test]
    fn clamp_holds_at_zero_when_content_fits() {
        let mut s = ScrollState { offset_y: -100.0, content_h: 100.0, viewport_h: 500.0 };
        s.clamp_offset();
        assert_eq!(s.offset_y, 0.0);
        assert_eq!(s.max_offset(), 0.0);
    }

    #[test]
    fn apply_delta_accumulates() {
        let id = ViewId(0xDEAD0001);
        set_scroll_state(id, ScrollState { offset_y: 0.0, content_h: 1000.0, viewport_h: 200.0 });
        let s1 = apply_scroll_delta(id, 50.0);
        assert_eq!(s1.offset_y, 50.0);
        let s2 = apply_scroll_delta(id, 30.0);
        assert_eq!(s2.offset_y, 80.0);
        forget(id);
    }

    #[test]
    fn apply_delta_clamps_at_bottom() {
        let id = ViewId(0xDEAD0002);
        set_scroll_state(id, ScrollState { offset_y: 0.0, content_h: 1000.0, viewport_h: 200.0 });
        apply_scroll_delta(id, 100_000.0);
        let s = scroll_state(id);
        assert_eq!(s.offset_y, 800.0); // 1000 - 200
        forget(id);
    }
}
