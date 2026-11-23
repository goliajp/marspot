//! The right-click context menu: what was clicked, the items it offers, and running the chosen one.

use super::*;

impl CoreApp {
    // ─── F3+9 — right-click context menu (split-arch L2 side) ────────

    pub(super) fn mouse_right_down(
        &mut self, wi: usize,
        x_phys: f64,
        y_phys: f64,
        _modifiers: Modifiers,
    ) {
        // An open panel has the pointer; no menu opens under it.
        if win!(self, wi).panels.owns_pointer() {
            return;
        }
        // Second right-click closes the previous menu first.
        if win!(self, wi).context_menu.take().is_some() {
            win!(self, wi).needs_render = true;
        }
        // Badge-prefix right-click: the menu CONTENT lives in the L1
        // plugin that owns the badge, so ask it (PaneBadgeMenuRequest)
        // and open the menu when the PaneBadgeMenu reply arrives —
        // same request/response shape as GetSelectionText.  Checked
        // ahead of the link / region paths so `P<n>` never opens the
        // generic pane menu.
        let on = self.chrome_at(wi, x_phys, y_phys);
        if let Some(ChromeTarget::Badge(i)) = on {
            if let Some(sid) = win!(self, wi).panes.get(i).and_then(|p| p.shelld_session_id()) {
                let payload = marspot::shell_proto::encode_pane_badge_menu_request(
                    sid, x_phys, y_phys,
                );
                lx_debug!(
                    "core.badge_menu.requested",
                    "badge right-click; asked L1 for the menu",
                    pane = i,
                    session = sid
                );
                self.pending_to_shell
                    .push((MsgType::PaneBadgeMenuRequest, payload));
                return;
            }
            // Hit the badge but the pane has no session to ask about.
            // Silent before: the click fell through to the generic
            // menu and looked like nothing happened.
            lx_warn!(
                "core.badge_menu.no_session",
                "badge hit but pane has no session id; falling through",
                pane = i
            );
        } else if let Some(miss) = self.badge_miss_report(wi, x_phys, y_phys) {
            // A right-click inside a title strip that shows a badge,
            // yet missed it.  Says where the badge was thought to be
            // versus where the click landed, because the geometry is
            // computed twice — here and in the renderer — from the
            // same inputs, and nothing was checking that they agree.
            lx_warn!("core.badge_hit_miss", &miss);
        }
        // A right-click that lands on a recognised URL / file path
        // gets a link-specific menu (Open / Copy) instead of the
        // generic pane menu.  Email is recognised but inert.
        let link = self.hit_test_link_at_xy(wi, x_phys, y_phys);
        if link.is_none() {
            // The same trap `core.badge_hit_miss` sets one screen up:
            // a link you can SEE and cannot click means the hit-test
            // and the render pass disagreed, and until now the only
            // symptom was a generic menu where a link menu belonged.
            // Says where the click landed, what the hit-test scanned,
            // and what it found on that row — enough to tell a column
            // mismatch from a scan that saw nothing at all.
            if let Some(r) = self.link_miss_report(wi, x_phys, y_phys) {
                lx_info!("core.link_hit_miss", &r);
            }
        }
        let region = self.resolve_context_region(wi, x_phys, y_phys);
        let items = match &link {
            Some(l) => self.build_link_menu_items(l),
            None => self.build_menu_items(wi, region),
        };
        if items.is_empty() {
            return;
        }
        win!(self, wi).context_menu = Some(ContextMenuState {
            items: items.into(),
            anchor_x: marspot::ui::Units::from_px(x_phys),
            anchor_y: marspot::ui::Units::from_px(y_phys),
            region,
            surface: Default::default(),
            link,
        });
        win!(self, wi).needs_render = true;
    }

    /// Open the badge context menu from a `PaneBadgeMenu` reply.  The
    /// anchor is the echoed right-click position; empty items = the
    /// owning plugin has nothing to offer, show nothing.
    pub(super) fn open_pane_badge_menu(
        &mut self, wi: usize,
        sid: u64,
        anchor_x: f64,
        anchor_y: f64,
        items: Vec<marspot::shell_proto::PaneBadgeMenuItem>,
    ) {
        if items.is_empty() {
            // The plugin that owns the badge had nothing to offer.
            // Silent until now, which made a dead right-click
            // indistinguishable from a missed hit-test.
            lx_warn!(
                "core.badge_menu_empty",
                &format!("shelld_session={sid} replied with no items")
            );
            return;
        }
        let helpers = self.pane_control.shadows.family.count(sid);
        use marspot::ui::components::MenuItem;
        let of_helpers = (helpers > 0)
            .then(|| MenuItem::entry(&strings::text(Arg::HelpersOf(helpers)), helper_panel::MENU_TAG));
        let items = items.into_iter().map(|it| MenuItem::entry(&it.label, it.tag)).chain(of_helpers).collect();
        win!(self, wi).context_menu = Some(ContextMenuState {
            items,
            anchor_x: marspot::ui::Units::from_px(anchor_x),
            anchor_y: marspot::ui::Units::from_px(anchor_y),
            region: ContextRegion::PaneBadge(sid),
            surface: Default::default(),
            link: None,
        });
        win!(self, wi).needs_render = true;
    }

    pub(super) fn resolve_context_region(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> ContextRegion {
        // Sidebar row check first — wins over the cell-area hit when
        // both overlap (the sidebar overlays the title-strip band).
        if let Some(idx) = self.chrome_at(wi, x_phys, y_phys).and_then(ChromeTarget::row) {
            return ContextRegion::SidebarSlot(idx);
        }
        if let Some(idx) = win!(self, wi).layout.hit_test(x_phys, y_phys) {
            return ContextRegion::Pane(idx);
        }
        ContextRegion::TitleStrip
    }

    pub(super) fn build_menu_items(
        &self, wi: usize,
        region: ContextRegion,
    ) -> Vec<marspot::ui::components::MenuItem> {
        use marspot::ui::components::MenuItem;
        use marspot::commands::{hint, Command};
        match region {
            ContextRegion::Pane(_) => {
                // Never disabled: closing a window's last pane closes
                // the window, and the last window's last pane quits.
                let close = MenuItem::entry(
                    t(Msg::MenuClosePane), ContextMenuAction::ClosePane.tag(),
                );
                let copy = MenuItem::entry(
                    t(Msg::MenuCopy), ContextMenuAction::CopySelection.tag(),
                ).with_shortcut(&hint(Command::Copy));
                let mut items = vec![
                    if win!(self, wi).selection.is_some() { copy } else { copy.disabled() },
                    MenuItem::entry(t(Msg::MenuPaste), ContextMenuAction::Paste.tag())
                        .with_shortcut(&hint(Command::Paste)),
                    MenuItem::divider(),
                    MenuItem::entry(t(Msg::MenuClearScrollback),
                        ContextMenuAction::ClearScrollback.tag()),
                    MenuItem::divider(),
                    MenuItem::entry(t(Msg::MenuNewPane),
                        ContextMenuAction::SplitNewPane.tag()),
                    close,
                ];
                items.push(MenuItem::divider());
                self.push_move_to_window_items(wi, &mut items);
                items
            }
            ContextRegion::SidebarSlot(_) => {
                let close = MenuItem::entry(
                    t(Msg::MenuClosePane), ContextMenuAction::ClosePane.tag(),
                );
                let mut items = vec![
                    MenuItem::divider(),
                    close,
                ];
                items.push(MenuItem::divider());
                self.push_move_to_window_items(wi, &mut items);
                items
            }
            ContextRegion::TitleStrip => vec![
                MenuItem::entry(t(Msg::MenuToggleSidebar),
                    ContextMenuAction::ToggleSidebar.tag())
                    .with_shortcut(&hint(Command::ToggleSidebar)),
                MenuItem::entry(t(Msg::MenuOpenLayout),
                    ContextMenuAction::OpenLayout.tag()),
            ],
            // Badge menus never come through here — their items
            // arrive from L1 via PaneBadgeMenu and open through
            // `open_pane_badge_menu`.
            ContextRegion::PaneBadge(_) => Vec::new(),
        }
    }

    /// Two-entry menu shown when the user clicks (either button) on
    /// a URL or file-path span.  Labels read per-kind so the user
    /// can tell from the menu what they're acting on.
    pub(super) fn build_link_menu_items(
        &self,
        link: &LinkContext,
    ) -> Vec<marspot::ui::components::MenuItem> {
        link_menu_items_for(link)
    }

    /// RFC-005 step 5 — the cross-window entries of a pane's context
    /// menu: one "Move to Window N" per OTHER window, plus "Move to
    /// New Window".  Window numbers are 1-based creation order, the
    /// same order the sidebar of each window implies.
    pub(super) fn push_move_to_window_items(
        &self,
        wi: usize,
        items: &mut Vec<marspot::ui::components::MenuItem>,
    ) {
        use marspot::ui::components::MenuItem;
        for (ti, w) in self.windows.iter().enumerate() {
            if ti == wi {
                continue;
            }
            items.push(MenuItem::entry(
                &strings::text(Arg::MoveToWindow(ti + 1)),
                MOVE_TO_WINDOW_TAG_BASE + ti as u32,
            ));
            let _ = w;
        }
        // Moving the only pane of the only window to a "new" window
        // would just rebuild the same state with a window flash.
        let pointless =
            self.windows.len() == 1 && win!(self, wi).panes.len() <= 1;
        let mi = MenuItem::entry(t(Msg::MenuMoveToNewWindow), MOVE_TO_NEW_WINDOW_TAG);
        items.push(if pointless { mi.disabled() } else { mi });
    }

    pub(super) fn dispatch_context_action(
        &mut self, wi: usize,
        action: ContextMenuAction,
        region: ContextRegion,
    ) {
        // Snapshot the link before clearing the menu — the OpenLink /
        // CopyLink arms read it after the clear.  Kind matters for
        // OpenLink (Email needs a `mailto:` prefix so `open(1)` routes
        // to the default mail client, not the browser).
        let link_snapshot = win!(self, wi)
            .context_menu
            .as_ref()
            .and_then(|s| s.link.clone());
        win!(self, wi).context_menu = None;
        match action {
            ContextMenuAction::CopySelection => {
                let _ = self.copy_selection_to_clipboard(wi);
            }
            ContextMenuAction::Paste => {
                if let Some(txt) = marspot::input::read_clipboard_text()
                    && let Some(pane) = win!(self, wi).try_focused_pane_mut() {
                        pane.session_mut().forward_paste(&txt);
                    }
            }
            ContextMenuAction::ClearScrollback => {
                if let Some(pane) = win!(self, wi).try_focused_pane_mut() {
                    pane.session_mut().forward_inject_input(b"\x1b[3J");
                }
            }
            ContextMenuAction::ClosePane => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => win!(self, wi).focused_idx,
                };
                // No floor on the pane count: closing the last pane of
                // a window closes the window, and closing the last
                // pane of the last window quits marspot.  Both are
                // driven from `close_session` → `close_window_if_emptied`.
                if idx < win!(self, wi).panes.len() {
                    self.close_session(wi, idx);
                    if wi < self.windows.len() {
                        self.rebuild_layout(wi);
                    }
                }
            }
            ContextMenuAction::SplitNewPane => self.add_pane(wi),
            ContextMenuAction::ToggleSidebar => self.toggle_sidebar(wi),
            ContextMenuAction::OpenLayout => {
                let change = win!(self, wi).panels.show(SurfaceKind::Layout);
                self.apply_surface_change(wi, change);
            }
            ContextMenuAction::OpenLink => {
                if let Some(link) = link_snapshot.as_ref() {
                    link_menu::open(link, false);
                }
            }
            ContextMenuAction::RevealLink => {
                if let Some(link) = link_snapshot.as_ref() {
                    link_menu::open(link, true);
                }
            }
            ContextMenuAction::CopyLink => {
                if let Some(link) = link_snapshot.as_ref() {
                    // The full path, not the four characters on
                    // screen: a relative path pasted elsewhere has to
                    // still name the same file.
                    let _ = marspot::input::write_clipboard_text(link.actionable());
                }
            }
        }
        win!(self, wi).needs_render = true;
    }
}
