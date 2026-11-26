//! Panels and modals: opening and closing surfaces, panel geometry and the usage modal.

use super::*;

#[path = "reset_panel.rs"]
mod reset_panel;

/// State behind the toolbar's accounts (usage) panel.
pub(super) struct CcUsageModalState {
    /// Parsed feed; `None` = file missing/unreadable (modal shows a
    /// placeholder instead of closing).
    pub(super) data: Option<marspot::cc_usage::CcUsage>,
    pub(super) codex: Option<marspot::cc_usage::CodexUsage>,
    pub(super) resets: Vec<marspot::agent_resets::AccountResets>,
    /// Each account's last reset claim, read with `resets`.
    pub(super) claims: Vec<marspot::agent_resets::Claimed>,
    /// The reset line armed by a first press.
    pub(super) buttons: marspot::agent_resets::ResetButtons,
    pub(super) loaded_at: Instant,
    pub(super) surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::cc_usage_view::CcUsageSurface>>,
}

impl CoreApp {
    /// A panel's toolbar button or shortcut.
    pub(super) fn toggle_panel(&mut self, wi: usize, kind: SurfaceKind) {
        let change = win!(self, wi).panels.on_toggle(kind);
        self.apply_surface_change(wi, change);
    }

    /// Drops the data of the panel that closed and readies the data of
    /// the one that opened.  Every way a panel opens or closes ends
    /// here, so each starts from the same state however it was reached.
    pub(super) fn apply_surface_change(&mut self, wi: usize, change: Change) {
        let w = &mut win!(self, wi);
        w.needs_render = true;
        w.panels.log_change(w.window_id, change);
        match change.closed.map(|(kind, _)| kind) {
            Some(SurfaceKind::Processes) => w.process_panel = None,
            Some(SurfaceKind::Accounts) => w.cc_usage_modal = None,
            Some(SurfaceKind::Layout) => w.layout_drag = None,
            Some(SurfaceKind::Helpers) => w.helpers_panel = None,
            Some(SurfaceKind::Shortcuts) | Some(SurfaceKind::Settings) | None => {}
        }
        match change.opened {
            Some(SurfaceKind::Settings) => {
                w.settings_modal_scroll = Default::default();
                w.settings_surface.borrow_mut().kept.surface.interaction.reset();
                // Pick up anything edited by hand since the last look, so
                // the panel never shows a value the file disagrees with.
                marspot::settings::reload_if_changed();
            }
            Some(SurfaceKind::Layout) => {
                // Staged values start from the live grid on every open.
                w.pending_grid_cols = w.grid_cols;
                w.pending_grid_rows = w.grid_rows;
                w.layout_surface.borrow_mut().kept.surface.interaction.reset();
                self.reset_card_slots(wi);
                // The card labels are directories: read them now, not
                // up to CWD_SWEEP_INTERVAL ago.
                let sids: Vec<u64> =
                    win!(self, wi).panes.iter().filter_map(|p| p.shelld_session_id()).collect();
                for sid in sids {
                    self.resolve_pane_cwd(sid);
                }
            }
            Some(SurfaceKind::Processes) => {
                w.process_panel = Some(ProcessPanelState::new());
                // Walked now so the first frame already has its rows.
                self.refresh_process_panel(wi);
            }
            Some(SurfaceKind::Accounts) => {
                w.cc_usage_modal = Some(CcUsageModalState {
                    data: marspot::cc_usage::read(),
                    codex: marspot::kimi_usage::read_codex_and_kimi(),
                    resets: marspot::agent_resets::read(unix_now()),
                    claims: marspot::agent_resets::last_claims(&marspot::agent_resets::claims_dir()),
                    buttons: Default::default(),
                    loaded_at: Instant::now(),
                    surface: Default::default(),
                });
            }
            Some(SurfaceKind::Helpers) => {
                let panel = self.new_helper_panel();
                win!(self, wi).helpers_panel = Some(panel);
            }
            Some(SurfaceKind::Shortcuts) => w.shortcuts_surface.borrow_mut().kept.surface.interaction.reset(),
            None => {}
        }
    }

    /// The frame of the open panel `kind`, where it has been dragged to.
    pub(super) fn panel_frame(&mut self, wi: usize, kind: SurfaceKind) -> marspot_term::layout::Rect {
        match kind {
            SurfaceKind::Settings => self.settings_modal_rect(wi),
            SurfaceKind::Layout => self.layout_modal_geometry(wi).chrome.frame,
            SurfaceKind::Accounts => self.cc_usage_modal_rect(wi),
            SurfaceKind::Shortcuts => self.shortcuts_panel_rect(wi),
            SurfaceKind::Helpers => self.helpers_rect(wi),
            SurfaceKind::Processes => {
                let (_, cell_h) = self.renderer.cell_dims();
                let w = &win!(self, wi);
                marspot::ui::components::process_panel::panel_rect(
                    w.w_phys, w.h_phys, cell_h, w.layout.top_inset, w.panels.offset(),
                )
            }
        }
    }

    /// A press on the content of the open panel `kind` — inside its
    /// frame, on neither its header nor its close button.
    pub(super) fn panel_content_press(&mut self, wi: usize, kind: SurfaceKind, x_phys: f64, y_phys: f64) {
        match kind {
            SurfaceKind::Accounts => self.reset_panel_press(wi, x_phys, y_phys),
            SurfaceKind::Settings | SurfaceKind::Shortcuts | SurfaceKind::Helpers => {
                self.tree_panel_content_press(wi, kind, x_phys, y_phys);
            }
            SurfaceKind::Processes => {
                let on = self.tree_panel_action_at(wi, kind, x_phys, y_phys);
                self.process_panel_do(wi, on.map_or(ProcessAsk::None, process_view::asked));
            }
            SurfaceKind::Layout => {
                let on = self.tree_panel_action_at(wi, kind, x_phys, y_phys);
                self.layout_panel_do(wi, on.map_or(LayoutAsk::None, layout_view::asked), (x_phys, y_phys));
            }
        }
    }

    /// The open layout panel's geometry, as last drawn.
    pub(super) fn layout_modal_geometry(&mut self, wi: usize) -> marspot::ui::components::LayoutModal {
        let w = &win!(self, wi);
        let (w_phys, h_phys, top_inset) = (w.w_phys, w.h_phys, w.layout.top_inset);
        let (cols, rows, label_w) = (w.pending_grid_cols, w.pending_grid_rows, w.layout_modal_label_w);
        let offset = w.panels.offset();
        let mut measure = marspot::ui::components::controls::font_measure(self.renderer.font_mut());
        marspot::ui::components::LayoutModal::layout_moved(
            w_phys, h_phys, top_inset, cols, rows, label_w, &mut measure, offset,
        )
    }

    pub(super) fn cc_usage_modal_rect(&self, wi: usize) -> marspot_term::layout::Rect {
        // Both feeds share the panel, so both feed its geometry.
        let modal = win!(self, wi).cc_usage_modal.as_ref();
        let (n, window_rows) = marspot::ui::components::cc_usage_modal::feed_shape(
            modal.and_then(|m| m.data.as_ref()),
            modal.and_then(|m| m.codex.as_ref()),
        );
        let reset_rows = modal.map_or(0, reset_rows);
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let w = &win!(self, wi);
        let home = marspot::ui::components::cc_usage_modal::panel_rect_with(
            (n, window_rows, reset_rows), w.w_phys, w.h_phys, cell_w, cell_h, w.layout.top_inset,
        );
        surface_stack::place(home, w.panels.offset(), w.w_phys, w.h_phys, w.layout.top_inset)
    }

    /// cc — build the `Cc` usage modal render data.  Re-reads the
    /// feed at most every 5 s while the modal is open.
    pub(super) fn build_cc_usage_render(&mut self, wi: usize) -> Option<marspot::ui::components::cc_usage_modal::CcUsageRender> {
        use marspot::ui::components::cc_usage_modal::{CcUsageAccountRender, CcUsageRender, Clock};
        let modal = win!(self, wi).cc_usage_modal.as_mut()?;
        if modal.loaded_at.elapsed() > std::time::Duration::from_secs(5) {
            modal.data = marspot::cc_usage::read();
            modal.codex = marspot::kimi_usage::read_codex_and_kimi();
            modal.resets = marspot::agent_resets::read(unix_now());
            modal.claims = marspot::agent_resets::last_claims(&marspot::agent_resets::claims_dir());
            modal.loaded_at = Instant::now();
            win!(self, wi).needs_render = true;
        }
        let rect = self.cc_usage_modal_rect(wi);
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let md_hm = |unix: i64| {
            let (month, day, hour, minute) = marspot::cc_usage::local_mdhm(unix);
            strings::MonthDayTime { month, day, hour, minute }
        };
        let fmt_hm = |unix: i64| -> String {
            let (_, _, h, mi) = marspot::cc_usage::local_mdhm(unix);
            format!("{h:02}:{mi:02}")
        };
        // Windows carry their own reset, so the bar-end labels are
        // formatted per window rather than per account.
        let window = |w: &marspot::cc_usage::CcWindow| marspot::ui::components::cc_usage_modal::CcUsageWindowRender {
            label: w.label.clone(),
            util: w.util.map(|u| u as f32),
            reset_unix: w.reset,
            span_secs: w.span_secs,
            reset_hm: w.reset.map(fmt_hm).unwrap_or_default(),
        };
        // The card's footer names one reset, and the useful one is the
        // window that turns over first — that is when the account can
        // be used again.
        let soonest_reset = |ws: &[marspot::cc_usage::CcWindow]| -> String {
            match ws.iter().filter_map(|w| w.reset).min() {
                Some(at) => strings::text(Arg::ResetAt(md_hm(at))),
                None => String::new(),
            }
        };
        let severity = |status: &str| match marspot::cc_usage::CcStatusKind::classify(status) {
            marspot::cc_usage::CcStatusKind::Ok => 0,
            marspot::cc_usage::CcStatusKind::Warn => 1,
            _ => 2,
        };

        let modal = win!(self, wi).cc_usage_modal.as_ref()?;
        let mut accounts: Vec<CcUsageAccountRender> = Vec::new();
        if let Some(d) = modal.data.as_ref() {
            accounts.extend(d.accounts.iter().map(|a| {
                let ws = a.windows();
                CcUsageAccountRender {
                    name: a.name.clone(),
                    email: a.email.clone(),
                    status_label: marspot::cc_usage::CcStatusKind::classify(&a.status)
                        .label(&a.status),
                    status_severity: severity(&a.status),
                    reset_label: soonest_reset(&ws),
                    windows: ws.iter().map(window).collect(),
                    resets: reset_panel::lines_for(modal, account_of(&modal.resets, &["claude"], &a.name), &ws, now_unix),
                }
            }));
        }
        if let Some(d) = modal.codex.as_ref() {
            accounts.extend(d.accounts.iter().map(|a| CcUsageAccountRender {
                name: a.name.clone(),
                email: a.email.clone(),
                status_label: marspot::cc_usage::CcStatusKind::classify(&a.status).label(&a.status),
                status_severity: severity(&a.status),
                reset_label: soonest_reset(&a.windows),
                windows: a.windows.iter().map(window).collect(),
                resets: reset_panel::lines_for(modal, account_of(&modal.resets, &["codex", "kimi"], &a.name), &a.windows, now_unix),
            }));
        }
        if accounts.is_empty() {
            return Some(CcUsageRender {
                rect,
                updated_label: String::new(),
                accounts,
                clock: Clock::local(now_unix),
                pointer: None,
                surface: modal.surface.clone(),
            });
        }
        // Two feeds, two generation stamps. The panel shows the older
        // one: a stamp is a promise about how stale the WHOLE panel is,
        // and the newer feed cannot vouch for the other's numbers.
        let generated_at = [
            modal.data.as_ref().map(|d| d.generated_at),
            modal.codex.as_ref().map(|d| d.generated_at),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0);
        Some(CcUsageRender {
            rect,
            updated_label: strings::text(Arg::UpdatedAt(md_hm(generated_at))),
            clock: Clock::local(now_unix),
            pointer: None,
            surface: modal.surface.clone(),
            accounts,
        })
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The account named `name` of one of `agents`.
fn account_of<'a>(all: &'a [marspot::agent_resets::AccountResets], agents: &[&str], name: &str) -> Option<&'a marspot::agent_resets::AccountResets> {
    all.iter().find(|a| a.name == name && agents.contains(&a.agent.as_str()))
}

/// The resets of the account named `name` of one of `agents`.
fn resets_of<'a>(all: &'a [marspot::agent_resets::AccountResets], agents: &[&str], name: &str) -> Option<&'a marspot::agent_resets::Resets> {
    account_of(all, agents, name).map(|a| &a.resets)
}

/// The most reset lines any account of the open panel lists.
fn reset_rows(modal: &CcUsageModalState) -> usize {
    let count = |agents: &[&str], name: &str| resets_of(&modal.resets, agents, name).map_or(0, marspot::agent_resets::line_count);
    let claude = modal.data.iter().flat_map(|d| d.accounts.iter()).map(|a| count(&["claude"], &a.name));
    let rest = modal.codex.iter().flat_map(|d| d.accounts.iter()).map(|a| count(&["codex", "kimi"], &a.name));
    claude.chain(rest).max().unwrap_or(0)
}
