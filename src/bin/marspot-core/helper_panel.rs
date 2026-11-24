//! The helper panel, and what the chrome says about shadow panes: the
//! `+N` after a pane's title and the sidebar's count.
//!
//! Both are derived from `shadows` and whose shadow each one is.  The
//! counts are worked out when the shadows change, the panel's rows when
//! the shadows, a title or the deny list do; how long a pane has run is
//! worked out when the panel is drawn.  Nothing here wakes the loop by
//! itself.
//!
//! The core moves a pane to a window itself; closing and denying are
//! the shell's (`HelperPanelAct`).

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use marspot::control_deny::Cover;
use marspot::fs_watch::Watcher;
use marspot::shell_proto::{CwdControl, HelperPanelAct, PanelAct, PanePlace, PlaceWhere, encode_helper_panel_act};
use marspot::ui::components::helpers_view::{self, Ask, Control, HelperGroup, HelperRow, HelpersRender, HelpersSurface};

use super::shadow_place::L2_REQ_UNTRACKED;
use super::*;

/// The badge menu's row that opens the panel: the core's own, outside
/// every plugin's range.
pub(super) const MENU_TAG: u32 = 0x6000_0000;
/// The badge menu's row that leaves the pane's view.
pub(super) const BACK_TAG: u32 = MENU_TAG + 1;

/// Shadow panes nest two deep by default; a chain that loops ends here.
const WALK_MAX: usize = 16;

/// Set by the deny list's watcher.
static DENY_CHANGED: AtomicBool = AtomicBool::new(false);

/// Whose shadow each shadow pane is, and what follows from it.
#[derive(Default)]
pub(super) struct Family {
    parents: HashMap<u64, u64>,
    /// Each pane with shadows below it that is no shadow itself, and
    /// how many.
    counts: Vec<(u64, u32)>,
    /// The counts are those of the shadows now.
    counted: bool,
    /// What the panel about to open is of.
    opening: u64,
}

impl Family {
    pub(super) fn adopted(&mut self, sid: u64, parent: u64) {
        self.parents.insert(sid, parent);
    }

    /// The pane at the top of `sid`'s chain of parents.
    pub(super) fn root(&self, sid: u64) -> u64 {
        let mut at = sid;
        for _ in 0..WALK_MAX {
            match self.parents.get(&at) {
                Some(p) => at = *p,
                None => break,
            }
        }
        at
    }

    pub(super) fn counted(&self) -> bool {
        self.counted
    }

    pub(super) fn count(&self, sid: u64) -> u32 {
        self.counts.iter().find(|c| c.0 == sid).map_or(0, |c| c.1)
    }
}

/// The open panel's data.
pub(super) struct HelperPanelState {
    /// The session it is of; 0: every session.
    of: u64,
    /// The rows last drawn: read again only for a frame.
    groups: Rc<Vec<HelperGroup>>,
    stale: bool,
    /// Where its top edge was first drawn, undragged, and in a window of
    /// what size: there it stays while its height changes.
    top: Option<(f64, (f64, f64))>,
    scroll: marspot::ui::Units,
    pub(super) surface: Rc<std::cell::RefCell<HelpersSurface>>,
    /// The deny list, watched while its state is on screen.
    _deny: Option<Watcher>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What the registry says of `sid`: its shell's pid and directory.
fn shell_of(sid: u64) -> (Option<i32>, String) {
    let Ok(e) = session_registry::read_session_entry(sid) else { return (None, String::new()) };
    let pid = (e.shell_child_pid > 1).then_some(e.shell_child_pid);
    let cwd = pid.and_then(marspot::pidtree::pane_cwd).map_or(e.cwd, |p| p.to_string_lossy().into_owned());
    (pid, cwd)
}

fn full_name(sid: u64, cwd: &str) -> String {
    marspot::pane_name::PaneRef::new(sid, cwd.to_string()).full_name()
}

impl CoreApp {
    /// A shadow pane came, went or moved: count again, and draw what
    /// says so.
    pub(super) fn shadows_changed(&mut self) {
        let sids: Vec<u64> = self.shadows.iter().filter_map(Pane::shelld_session_id).collect();
        let fam = &mut self.pane_control.shadows.family;
        fam.parents.retain(|sid, _| sids.contains(sid));
        for sid in &sids {
            // one the registry alone knows about: adopted at boot
            if let std::collections::hash_map::Entry::Vacant(v) = fam.parents.entry(*sid)
                && let Ok(e) = session_registry::read_session_entry(*sid)
                && e.shadow_of != 0
            {
                v.insert(e.shadow_of);
            }
        }
        fam.counts.clear();
        for sid in sids {
            let root = fam.root(sid);
            match fam.counts.iter_mut().find(|c| c.0 == root) {
                Some(c) => c.1 += 1,
                None => fam.counts.push((root, 1)),
            }
        }
        fam.counted = true;
        for w in &mut self.windows {
            w.needs_render = true;
        }
        self.helper_rows_changed();
    }

    /// What an open helper panel shows is to be read again.
    pub(super) fn helper_rows_changed(&mut self) {
        for w in &mut self.windows {
            if let Some(p) = w.helpers_panel.as_mut() {
                p.stale = true;
                w.needs_render = true;
            }
        }
    }

    /// Once per loop turn: the counts at boot, and a deny list that
    /// changed under an open panel.
    pub(super) fn helper_panel_tick(&mut self) {
        if !self.pane_control.shadows.family.counted {
            self.shadows_changed();
        }
        if DENY_CHANGED.swap(false, Ordering::AcqRel) {
            self.helper_rows_changed();
        }
    }

    /// Open the panel for session `of`, or for every session (0).
    pub(super) fn open_helpers(&mut self, wi: usize, of: u64) {
        self.pane_control.shadows.family.opening = of;
        let change = win!(self, wi).panels.show(SurfaceKind::Helpers);
        self.apply_surface_change(wi, change);
    }

    /// The data of the panel that just opened.
    pub(super) fn new_helper_panel(&mut self) -> HelperPanelState {
        let wake = self.event_tx.clone();
        let deny = Watcher::spawn(&[marspot::control_deny::file_of(&marspot::control_deny::path())], move || {
            DENY_CHANGED.store(true, Ordering::Release);
            drop(wake.send(CoreEvent::L3Ready));
        });
        if let Err(e) = &deny {
            lx_warn!("core.helper_panel.deny_unwatched", &format!("{e}; shown as it was when the panel opened"));
        }
        HelperPanelState {
            of: std::mem::take(&mut self.pane_control.shadows.family.opening),
            groups: Default::default(),
            stale: true,
            top: None,
            scroll: Default::default(),
            surface: Default::default(),
            _deny: deny.ok(),
        }
    }

    /// The sessions with shadow panes, the one `of` alone when it is
    /// not 0, each with its rows.
    fn helper_groups(&self, of: u64) -> Vec<HelperGroup> {
        let fam = &self.pane_control.shadows.family;
        let mut deny = marspot::control_deny::DenyList::at(marspot::control_deny::path());
        let roots = fam.counts.iter().map(|c| c.0).filter(|r| of == 0 || *r == of);
        roots
            .map(|root| {
                let cwd = shell_of(root).1;
                let rows = self.shadows.iter().filter_map(Pane::shelld_session_id).filter(|sid| fam.root(*sid) == root);
                HelperGroup {
                    parent: root,
                    name: full_name(root, &cwd),
                    control: match deny.cover(&cwd) {
                        Cover::Free => Control::Allowed,
                        Cover::Own => Control::Denied,
                        Cover::Above(_) => Control::DeniedAbove,
                    },
                    rows: rows.map(|sid| self.helper_row(sid)).collect(),
                }
            })
            .collect()
    }

    fn helper_row(&self, sid: u64) -> HelperRow {
        let (pid, cwd) = shell_of(sid);
        HelperRow {
            sid,
            parent: self.pane_control.shadows.family.parents.get(&sid).copied().unwrap_or(0),
            name: full_name(sid, &cwd),
            doing: self.pane_titles.get(&sid).cloned().unwrap_or(cwd),
            started_unix: pid.and_then(marspot::pidtree::proc_row).map(|r| r.start_us / 1_000_000),
        }
    }

    /// Read the open panel's rows again if they changed, for the frame
    /// about to show them and for nothing else: until then a press is on
    /// the rows on screen.
    fn refresh_helpers(&mut self, wi: usize) {
        let stale = win!(self, wi).helpers_panel.as_ref().filter(|p| p.stale).map(|p| p.of);
        let Some(of) = stale else { return };
        let groups = Rc::new(self.helper_groups(of));
        if let Some(p) = win!(self, wi).helpers_panel.as_mut() {
            p.groups = groups;
            p.stale = false;
        }
    }

    /// The open panel's frame, of the rows last drawn.
    pub(super) fn helpers_rect(&mut self, wi: usize) -> marspot_term::layout::Rect {
        let mut m = marspot::ui::components::controls::font_measure(self.renderer.font_mut());
        let w = &mut win!(self, wi);
        let groups = w.helpers_panel.as_ref().map(|p| Rc::clone(&p.groups)).unwrap_or_default();
        let mut home = helpers_view::panel_rect(w.w_phys, w.h_phys, &groups, w.layout.top_inset, &mut m);
        let window = (w.w_phys, w.h_phys);
        if let Some((top, _)) = w.helpers_panel.as_ref().and_then(|p| p.top).filter(|t| t.1 == window) {
            home = helpers_view::kept_at(home, top, w.h_phys, w.layout.top_inset);
        }
        let rect = surface_stack::place(home, w.panels.offset(), w.w_phys, w.h_phys, w.layout.top_inset);
        if let Some(p) = w.helpers_panel.as_mut() {
            if !p.stale && p.top.is_none_or(|t| t.1 != window) {
                p.top = Some((home.y_top, window));
            }
            let max = helpers_view::max_scroll(rect, &groups, &mut m);
            p.scroll = marspot::ui::Units::from_px(p.scroll.px().clamp(0.0, max));
        }
        rect
    }

    /// What a frame of the open panel is drawn from.
    fn helpers_render(&mut self, wi: usize) -> Option<HelpersRender> {
        win!(self, wi).helpers_panel.as_ref()?;
        self.refresh_helpers(wi);
        let rect = self.helpers_rect(wi);
        let p = win!(self, wi).helpers_panel.as_ref()?;
        Some(HelpersRender {
            rect,
            scroll: p.scroll.px(),
            groups: Rc::clone(&p.groups),
            now_unix: now_unix(),
            pointer: None,
            surface: Rc::clone(&p.surface),
        })
    }

    /// Give the frame being built the panel, and `wr` the counts its
    /// chrome shows.
    pub(super) fn publish_helpers(&mut self, wi: usize, wr: &mut marspot::render_metal::WindowRender) {
        let panel = self.helpers_render(wi);
        self.renderer.set_helpers_panel(panel);
        let fam = &self.pane_control.shadows.family;
        let panes = win!(self, wi).panes.iter();
        wr.set_helpers(
            self.shadows.len() as u32,
            panes.map(|p| p.shelld_session_id().filter(|sid| !self.strips.drawn(*sid)).map_or(0, |sid| fam.count(sid))),
        );
    }

    pub(super) fn helpers_scroll(&mut self, wi: usize, dy_phys: f64) {
        let rect = self.helpers_rect(wi);
        let mut m = marspot::ui::components::controls::font_measure(self.renderer.font_mut());
        let w = &mut win!(self, wi);
        let Some(p) = w.helpers_panel.as_mut() else { return };
        let max = helpers_view::max_scroll(rect, &p.groups, &mut m);
        p.scroll = marspot::ui::Units::from_px((p.scroll.px() + dy_phys).clamp(0.0, max));
        w.needs_render = true;
    }

    /// Do what a button of the panel asked for.
    pub(super) fn helpers_do(&mut self, wi: usize, ask: Ask) {
        // of the frame that was pressed
        let Some(groups) = win!(self, wi).helpers_panel.as_ref().map(|p| p.surface.borrow().groups()) else { return };
        let row = |i: usize| groups.iter().flat_map(|g| &g.rows).nth(i);
        let mut want = CwdControl::Flip;
        let (sid, act) = match ask {
            Ask::None => return,
            Ask::Move(i) => {
                if let Some(r) = row(i) {
                    self.move_helper_to_window(r.sid, r.parent);
                }
                return;
            }
            Ask::View(i) => {
                if let Some(sid) = row(i).map(|r| r.sid) {
                    self.view_from_panel(wi, sid);
                }
                return;
            }
            Ask::Close(i) => (row(i).map(|r| r.sid), PanelAct::CloseHelper),
            Ask::CloseAll(g) => (groups.get(g).map(|g| g.parent), PanelAct::CloseAllOfParent),
            Ask::Control(g) => {
                let Some(g) = groups.get(g).filter(|g| g.control != Control::DeniedAbove) else { return };
                want = if g.control == Control::Denied { CwdControl::Allow } else { CwdControl::Deny };
                (Some(g.parent), PanelAct::ToggleCwdControl)
            }
        };
        let Some(sid) = sid else { return };
        lx_event!("HELPER_PANEL_ASKED", "the user pressed a button of the helper panel", session = sid, act = format!("{act:?}"));
        self.pending_to_shell.push((MsgType::HelperPanelAct, encode_helper_panel_act(&HelperPanelAct { sid, act, want })));
    }

    /// In its parent's place, with the panel out of the way.
    fn view_from_panel(&mut self, wi: usize, sid: u64) {
        let host = self.pane_control.shadows.family.root(sid);
        if self.view_helper(host, Some(sid)) {
            let change = win!(self, wi).panels.close(CloseBy::Replaced);
            self.apply_surface_change(wi, change);
        }
    }

    /// Into the window of its parent; the answer is nobody's.
    fn move_helper_to_window(&mut self, sid: u64, parent: u64) {
        let m = PanePlace { req_id: L2_REQ_UNTRACKED, sid, to: PlaceWhere::Window, host_sid: parent, force: false };
        let route = self.place_pane(m);
        lx_event!("HELPER_PANEL_MOVE", "the user moved a shadow pane to a window", session = sid, route = format!("{route:?}"));
    }

    /// A row of a badge's menu was picked: the core's own, or one whose
    /// tag the plugin that made it knows.
    pub(super) fn pick_badge_row(&mut self, wi: usize, sid: u64, tag: u32) {
        win!(self, wi).context_menu = None;
        win!(self, wi).needs_render = true;
        if tag == MENU_TAG {
            self.open_helpers(wi, sid);
            return;
        }
        if tag == BACK_TAG {
            self.view_helper(sid, None);
            return;
        }
        let payload = marspot::shell_proto::encode_pane_badge_menu_action(sid, tag);
        self.pending_to_shell.push((MsgType::PaneBadgeMenuAction, payload));
    }

    /// Dev seam (`MARSPOT_DEV_PANELS`): where the first pane's `+N` is,
    /// then the first button of each kind on the open panel.
    pub(super) fn dev_helper_spots(&mut self, wi: usize) -> [Option<(f64, f64)>; 6] {
        let mut out = [None; 6];
        if self.shadows.is_empty() || !self.dev_panels.as_ref().is_some_and(|s| s.wants_helpers()) {
            return out;
        }
        if let Some(c) = win!(self, wi).layout.cells.first() {
            let (x, y, w) = (c.x, c.y_top + 4.0, c.w as usize);
            out[0] = (0..w)
                .map(|dx| (x + dx as f64, y))
                .find(|p| matches!(self.chrome_at(wi, p.0, p.1), Some(ChromeTarget::Helpers(0))));
        }
        if win!(self, wi).helpers_panel.is_some() {
            let r = self.helpers_rect(wi);
            let at = |i: usize, n: f64| i as f64 * 4.0 + n + 2.0;
            for (iy, ix) in (0..(r.h / 4.0) as usize).flat_map(|iy| (0..(r.w / 4.0) as usize).map(move |ix| (iy, ix))) {
                let p = (at(ix, r.x), at(iy, r.y_top));
                let spot = match self.helper_ask_at(wi, p) {
                    Ask::Move(0) => 1,
                    Ask::Close(0) => 2,
                    Ask::CloseAll(0) => 3,
                    Ask::Control(0) => 4,
                    Ask::View(0) => 5,
                    _ => continue,
                };
                out[spot].get_or_insert(p);
            }
        }
        out
    }

    fn helper_ask_at(&mut self, wi: usize, p: (f64, f64)) -> Ask {
        let surface = win!(self, wi).helpers_panel.as_ref().map(|s| Rc::clone(&s.surface));
        let hit = surface.and_then(|s| s.borrow().kept.surface.hit(p));
        hit.map_or(Ask::None, |h| helpers_view::asked(h.action))
    }
}

#[cfg(test)]
#[path = "helper_panel_tests.rs"]
mod tests;
