//! The pointer over a window: a move and a press.

use super::*;

impl CoreApp {
    pub(super) fn mouse_moved(&mut self, wi: usize, x_phys: f64, y_phys: f64) {
        self.strips.pointer_at(win!(self, wi).window_id, x_phys, y_phys);
        if self.pointer_left(wi, x_phys, y_phys) {
            return;
        }
        // Motion, for a program that asked to hear about it.
        //
        // Only the mode it asked for: `?1002` wants travel while a
        // button is held and `?1003` wants all of it, and sending the
        // second to a program expecting the first buries it in reports
        // per pixel of idle pointer movement.  Reported once per cell
        // the pointer enters, not once per event, for the same reason.
        // Chrome that floats over the panes owns the pointer while it
        // is up.  `mouse_down` has always checked the menu first; this
        // did not, so moving inside an open badge menu was reported to
        // the program underneath as pointer travel and returned before
        // the menu's own hover ran.  The menu still took clicks -- it
        // just stopped following the mouse.
        let chrome_owns_pointer = {
            let w = &win!(self, wi);
            w.context_menu.is_some() || w.panels.owns_pointer()
        };
        let held = self.mouse_report_press.is_some();
        if !chrome_owns_pointer
            && let Some((idx, col, row)) = self.cell_pos_at(wi, x_phys, y_phys)
            && self.shown_pane(wi, idx)
                .map(|p| p.session().l3_mouse_motion_wanted(held))
                .unwrap_or(false)
        {
            let (c1, r1) = (col as u32 + 1, row as u32 + 1);
            if self.mouse_report_last_cell != Some((wi, idx, c1, r1)) {
                self.mouse_report_last_cell = Some((wi, idx, c1, r1));
                let button = if held { 0 } else { 3 };
                if let Some(pane) = self.shown_pane_mut(wi, idx) {
                    pane.session_mut().l3_inject_mouse_motion(button, c1, r1);
                }
            }
            return;
        }

        self.hover_panels(wi, x_phys, y_phys);

        self.hover_chrome(wi, x_phys, y_phys);
    }

    pub(super) fn mouse_down(&mut self, wi: usize, x_phys: f64, y_phys: f64, modifiers: Modifiers) {
        self.strip_keys_interrupted();
        if self.menu_takes_press(wi, x_phys, y_phys) || self.close_ask_press(wi, x_phys, y_phys) {
            return;
        }

        let on = self.chrome_at(wi, x_phys, y_phys);
        if self.panel_takes_press(wi, on, x_phys, y_phys) {
            return;
        }

        // The sidebar button, the add button, a close button and a
        // badge's prefix act when they are let go on; a row and a strip
        // act on the press, below.
        if on.is_some() {
            self.press_chrome(wi, x_phys, y_phys);
        }
        if on.is_some_and(ChromeTarget::acts_on_release) {
            return;
        }
        if on.is_none() && (self.strip_press(wi, x_phys, y_phys) || self.scrollbar_press(wi, x_phys, y_phys)) {
            return;
        }
        let title_hit = match on {
            Some(ChromeTarget::Title(i)) => Some(i),
            _ => None,
        };
        let sidebar_hit = on.and_then(ChromeTarget::row);
        let cell_hit = win!(self, wi).layout.hit_test(x_phys, y_phys);
        let cell_pos_hit = self.cell_pos_at(wi, x_phys, y_phys);

        // Title-strip press: could be a click (enter title edit) or
        // the start of a pane drag (RFC-005 step 5).  Arm the drag and
        // Focus shifts immediately (clicking focuses); a drag from the
        // title strip moves the pane.
        if let Some(idx) = title_hit
            && self.title_press(wi, idx, x_phys, y_phys)
        {
            return;
        }

        if self.link_press(wi, x_phys, y_phys) || self.vacant_press(wi, cell_pos_hit) {
            return;
        }

        if self.reported_press(wi, cell_pos_hit, modifiers) {
            return;
        }

        // Click in cell body → start a fresh selection there AND
        // focus that cell.
        let prior_selection = win!(self, wi).selection;
        win!(self, wi).selection = None;
        win!(self, wi).selection_dragging = false;
        if let Some((idx, col, row)) = cell_pos_hit
            && self.start_selection(wi, idx, col, row, modifiers)
        {
            return;
        }
        if prior_selection.is_some() {
            win!(self, wi).needs_render = true;
        }

        if let Some(row) = sidebar_hit {
            self.sidebar_row_pressed(wi, row);
            return;
        }
        let new_focus = cell_hit;
        if let Some(idx) = new_focus {
            // F3+3.0 — click on an empty cell (idx >= panes.len(),
            // which means the grid has more cells than sessions
            // after a layout grow) spawns a new session and focuses
            // it.  Matches the sidebar [+] behaviour but lands the
            // user directly in the cell they clicked, so growing
            // the grid + filling it reads as one motion.
            if idx >= win!(self, wi).panes.len() && cell_hit.is_some() {
                if win!(self, wi).panes.len() < SESSION_COUNT_HARD_CAP {
                    self.spawn_session(wi);
                    win!(self, wi).focused_idx = win!(self, wi).panes.len() - 1;
                    self.rebuild_layout(wi);
                }
                return;
            }
            self.focus_pane(wi, idx);
        }
    }
}
