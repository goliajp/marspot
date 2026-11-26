//! Dev-panel driving: scripted drags, settings, panels and pane closes.

use super::pane_control::Route;
use super::*;

impl CoreApp {
    /// Dev seam (`MARSPOT_DEV_DRAG`) — see the call site.
    ///
    /// Waits until the windows the ending needs have panes laid out
    /// (restored panes land off-loop, seconds after boot), then plays
    /// the event sequence L1 would send for a real drag, in the order
    /// the event loop routes it: press on the title of window 0's first
    /// pane, move past the slop, move over the target, release.  Nothing
    /// below `mouse_down` / `mouse_drag` / `mouse_up` is bypassed — the
    /// ghost, the drop resolution and the landing are the production
    /// ones.
    pub(super) fn dev_drive_drag(&mut self) {
        let Some((ending, at)) = self.dev_drag else { return };
        if Instant::now() < at {
            return;
        }
        let needed = match ending {
            DevDrag::SplitRight => 2,
            DevDrag::NewWindow => 1,
        };
        let laid_out = |app: &Self, wi: usize| {
            !win!(app, wi).panes.is_empty() && !win!(app, wi).layout.cells.is_empty()
        };
        if self.windows.len() < needed || !(0..needed).all(|wi| laid_out(self, wi)) {
            return;
        }
        self.dev_drag = None;

        let src = win!(self, 0).layout.cells[0];
        let press = (src.x + src.w / 2.0, src.y_top + win!(self, 0).layout.cell_title_h / 2.0);
        let from_id = win!(self, 0).window_id;
        let (drop_id, drop) = match ending {
            DevDrag::SplitRight => {
                let dst = win!(self, 1).layout.cells[0];
                // Inside the right band, clear of the window edge.
                (win!(self, 1).window_id, (dst.x + dst.w - 8.0, dst.y_top + dst.h / 2.0))
            }
            // Window id 0 is "no marspot window under the pointer"; the
            // point is in screen space, where the new window is placed.
            DevDrag::NewWindow => (0, (640.0, 480.0)),
        };
        lx_event!(
            "DEV_DRAG",
            "driving a pane drag (dev seam)",
            ending = format!("{ending:?}"),
            from_window = from_id,
            drop_window = drop_id,
            windows_before = self.windows.len() as u32
        );

        let Some(wi) = self.focus_window(from_id) else { return };
        self.drag_window = Some(from_id);
        self.mouse_down(wi, press.0, press.1, Modifiers::default());
        let past_slop = press.0 + 2.0 * PANE_DRAG_SLOP * marspot::ui::chrome_scale();
        self.mouse_drag(wi, past_slop, press.1, from_id, past_slop, press.1);
        self.mouse_drag(wi, drop.0, drop.1, drop_id, drop.0, drop.1);
        lx_event!(
            "DEV_DRAG_HOVER",
            "ghost resolved before release",
            target = format!("{:?}", self.drop_target),
            drag_active = self.pane_drag.is_some_and(|d| d.active) as u32
        );
        if let Some(wi) = self.drag_target(from_id) {
            self.mouse_up(wi, drop_id, drop.0, drop.1);
        }
        self.drag_window = None;
        lx_event!(
            "DEV_DRAG_DONE",
            "pane drag released (dev seam)",
            windows_after = self.windows.len() as u32
        );
    }

    /// Dev seam (`MARSPOT_DEV_OPEN_SETTINGS`) — see the call site.
    pub(super) fn dev_drive_open_settings(&mut self) {
        match self.dev_open_settings_at {
            Some(t) if Instant::now() >= t => {}
            _ => return,
        }
        self.dev_open_settings_at = None;
        let wi = self.key_window.min(self.windows.len().saturating_sub(1));
        self.toggle_panel(wi, SurfaceKind::Settings);
    }

    /// Dev seam (`MARSPOT_DEV_PANELS`): queues the next step of the
    /// script as the events the window would have sent for it, so they
    /// are routed and handled like any others.
    pub(super) fn dev_drive_panels(&mut self) {
        let now = Instant::now();
        if !self.dev_panels.as_ref().is_some_and(|s| s.due(now)) {
            return;
        }
        let wi = self.key_window.min(self.windows.len().saturating_sub(1));
        let top = win!(self, wi).layout.top_inset;
        let chrome =
            win!(self, wi).panels.open().map(|k| PanelChrome::new(self.panel_frame(wi, k), top));
        let cell = self.renderer.cell_dims();
        let helpers = self.dev_helper_spots(wi);
        let strip = self.dev_strip_spot(wi);
        let w = &win!(self, wi);
        let id = w.window_id;
        let Some(script) = self.dev_panels.as_mut() else { return };
        for input in script.play(now, &Scene { layout: &w.layout, chrome, cell, helpers, strip }) {
            let _ = self.event_tx.send(match input {
                DevInput::Move(x, y) => CoreEvent::MouseMove(x, y, id),
                DevInput::Down(x, y, mods) => CoreEvent::MouseDown(x, y, mods, id),
                DevInput::Drag(x, y) => CoreEvent::MouseDrag(x, y, id, id, x, y),
                DevInput::Up(x, y) => CoreEvent::MouseUp(id, id, x, y),
                DevInput::Scroll(dy) => CoreEvent::Scroll(dy, true, id),
                DevInput::Key(event, mods) => CoreEvent::Key(event, mods, id),
            });
        }
        if script.finished() {
            self.dev_panels = None;
        }
    }

    /// Dev seam (`MARSPOT_DEV_CLOSE_PANES=n`) — see the call site.
    pub(super) fn dev_drive_close_panes(&mut self) {
        if self.dev_close_panes == 0 {
            return;
        }
        match self.dev_close_panes_at {
            Some(t) if Instant::now() < t => return,
            _ => {}
        }
        self.dev_close_panes -= 1;
        self.dev_close_panes_at = Some(Instant::now() + Duration::from_secs(1));
        let wi = self.key_window.min(self.windows.len().saturating_sub(1));
        let idx = win!(self, wi).focused_idx;
        lx_event!(
            "DEV_CLOSE_PANE",
            "closing a pane (dev seam)",
            window_id = win!(self, wi).window_id,
            pane_idx = idx as u32,
            remaining = self.dev_close_panes as u32
        );
        self.close_session(wi, idx);
        if wi < self.windows.len() {
            self.rebuild_layout(wi);
        }
    }

    /// Dev seam (`MARSPOT_DEV_SHADOWS=n`): open n shadow panes under
    /// the first live pane, one per loop turn, as L1's `PaneSpawn`
    /// would.  Request ids from the top of L1's range; L1 drops the
    /// answers as answers to nothing it asked.
    pub(super) fn dev_drive_shadows(&mut self) {
        let Some((n, at)) = self.pane_control.shadows.dev else { return };
        if Instant::now() < at {
            return;
        }
        let parent = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .find_map(|p| p.session().l3_session_id());
        let Some(parent) = parent else { return };
        self.pane_control.shadows.dev = (n > 1).then_some((n - 1, at));
        let route = self.dev_open_shadow(u32::MAX - 2 - n as u32, parent);
        lx_event!(
            "DEV_SHADOW",
            "opened a shadow pane (dev seam)",
            parent = parent,
            left = (n - 1) as u32,
            route = format!("{route:?}")
        );
    }

    /// Dev seam and tests: open one shadow pane under `parent`, through
    /// the same path as L1's request.  Its answer goes to L1 as any other.
    pub(super) fn dev_open_shadow(&mut self, req_id: u32, parent: u64) -> Route {
        let m = marspot::shell_proto::PaneSpawn {
            req_id,
            shadow: true,
            no_focus: true,
            helper: false,
            new_window: false,
            parent_sid: parent,
            idle_s: 0,
            cols: 0,
            rows: 0,
            cwd: String::new(),
        };
        match marspot::shell_proto::encode_pane_spawn(&m) {
            Ok(p) => self.route_shadow_request(Frame::new(MsgType::PaneSpawn, p), req_id),
            Err(_) => Route::Unanswerable,
        }
    }
}
