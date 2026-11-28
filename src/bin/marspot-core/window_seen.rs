//! What the shell says about whether the user can see a window: the
//! app as a whole gained or lost focus, or one window was minimized to
//! the Dock or came back.  A pane shown only in a minimized window is
//! on no screen, so its sidebar dot is drawn hollow like a pane in no
//! window.

use super::*;

/// Minimized windows remembered at most.  The shell names only windows
/// it has, and one named before the core has taken it on is kept too
/// (a core that takes over hears of it right after its surface pair);
/// a window leaves the list when it closes.
const MINIMIZED_CAP: usize = 64;

impl CoreApp {
    /// The whole app gained or lost focus: every window's chrome
    /// changes and every window owes a frame.
    pub(super) fn app_focus(&mut self, focused: bool) {
        self.renderer.set_window_focused(focused);
        for wi in 0..self.windows.len() {
            // no mouseMoved deliveries while the app is not active, so
            // a latched hover would linger
            if !focused {
                self.clear_hover(wi);
            }
            self.windows[wi].needs_render = true;
        }
    }

    /// Window `window_id` was minimized, or came back.
    pub(super) fn window_minimized(&mut self, window_id: u32, minimized: bool) {
        let list = &mut self.pane_control.shadows.minimized;
        list.retain(|id| *id != window_id);
        if minimized {
            if list.len() >= MINIMIZED_CAP {
                list.remove(0);
            }
            list.push(window_id);
        }
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
        lx_event!("WINDOW_MINIMIZED_SEEN", "the shell said a window was minimized or restored",
            window_id = window_id, minimized = minimized as u32);
    }

}

#[cfg(test)]
#[path = "window_seen_tests.rs"]
mod tests;
