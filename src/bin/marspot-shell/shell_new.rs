//! Building the ShellApp at launch: the binary tree, the plugin host and
//! registry, and the channels the supervisor drains.

use super::*;

impl ShellApp {
    pub(super) fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        let (pane_badge_tx, pane_badge_rx) = std::sync::mpsc::channel();
        let (pane_wheel_keys_tx, pane_wheel_keys_rx) = std::sync::mpsc::channel();
        let (pane_agent_tui_tx, pane_agent_tui_rx) = std::sync::mpsc::channel();
        let (pane_render_markup_tx, pane_render_markup_rx) = std::sync::mpsc::channel();
        let (pane_title_tx, pane_title_rx) = std::sync::mpsc::channel();
        let (pane_session_begin_tx, pane_session_begin_rx) = std::sync::mpsc::channel();
        let pane_session_begin_tx_for_ops = pane_session_begin_tx.clone();
        let (pty_op_tx, pty_op_rx) = std::sync::mpsc::channel();
        let (cli_tx, cli_rx) = std::sync::mpsc::channel();
        // Best-effort: a shell that cannot bind still runs the
        // terminal, it just cannot be asked to type into it.
        let wake_for_cli: cli_socket::Wake = {
            let p = proxy.clone();
            std::sync::Arc::new(move || p.wake())
        };
        if let Err(e) = cli_socket::serve(cli_tx, wake_for_cli) {
            lx_warn!("shell.cli.bind_failed", &format!("{e}"));
        }
        let wake_for_front = proxy.clone();
        instance::serve_front(move || wake_for_front.wake());
        // Best-effort like the command socket above: a shell that
        // cannot bind this one still runs the terminal, and submits
        // fall back to reading the pane.
        // The secret both layers derive pane tokens from. L1 makes it
        // because L1 owns the state root; L3 only reads it, so that
        // spawning a pane stays free of filesystem side effects.
        marspot_term::submit_receipt::ensure_secret();
        let receipts = receipts::Receipts::new();
        if let Err(e) = receipts::serve(std::sync::Arc::clone(&receipts)) {
            lx_warn!("shell.receipt.bind_failed", &format!("{e}"));
        }
        let (inject_input_tx, inject_input_rx) = std::sync::mpsc::channel();
        // The queue's own route to a PTY: the same channel plugins use,
        // so nothing has a private path to a pane.
        let inject_input_tx_for_ops = inject_input_tx.clone();
        let (pane_activity_tx, pane_activity_rx) = std::sync::mpsc::channel();
        // Cloned before the move: the plugin host gets its own knock.
        let wake_for_host: std::sync::Arc<dyn Fn() + Send + Sync> = {
            let p = proxy.clone();
            std::sync::Arc::new(move || p.wake())
        };
        Self {
            proxy,
            // The boot window is always entry 0 of both saved lists.
            windows: vec![ShellWindow::new(
                marspot::shell_proto::FIRST_WINDOW_ID,
                0,
            )],
            wake_queue: std::collections::VecDeque::new(),
            wake_next_at: None,
            app_focused: true,
            pending_frame_index: std::collections::HashMap::new(),
            next_window_id: marspot::shell_proto::FIRST_WINDOW_ID + 1,
            active: None,
            pump: None,
            binaries,
            safe_mode: std::env::var("MARSPOT_SAFE_MODE").is_ok(),
            dev_close_extra: false,
            dev_close_sequence: std::env::var("MARSPOT_DEV_CLOSE_SEQUENCE")
                .ok()
                .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
                .unwrap_or_default(),
            dev_badge_menu_file: std::env::var_os("MARSPOT_DEV_BADGE_MENU")
                .map(std::path::PathBuf::from),
            dev_close_next_at: None,
            dev_quit_after_paint: std::env::var("MARSPOT_DEV_QUIT_AFTER_PAINT")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_quit_at: None,
            core_generation: 0,
            sup_state: SupervisorState::Idle,
            probe_rx: None,
            crashes: std::collections::VecDeque::new(),
            auto_restart_disabled: false,
            disabled_at: None,
            budget_trips: 0,
            last_tick_at: None,
            banner_kind: None,
            core_boot_ring: std::collections::VecDeque::with_capacity(16),
            plugin_host: {
                let h = ShellPluginHost::new();
                h.attach_pane_badge_tx(pane_badge_tx);
                h.attach_pane_wheel_keys_tx(pane_wheel_keys_tx);
                h.attach_pane_agent_tui_tx(pane_agent_tui_tx);
                h.attach_pane_render_markup_tx(pane_render_markup_tx);
                h.attach_pane_title_tx(pane_title_tx);
                h.attach_pane_session_begin_tx(pane_session_begin_tx);
                h.attach_pty_op_tx(pty_op_tx);
                h.attach_inject_input_tx(inject_input_tx);
                h.attach_pane_activity_tx(pane_activity_tx);
                h.attach_wake(wake_for_host);
                h
            },
            plugin_registry: PluginRegistry::new(),
            last_plugin_tick: Instant::now() - Duration::from_secs(1),
            pane_look: pane_decls::PaneLook::new(),
            pane_status: {
                // Idle is a property of the pane, not of this process:
                // pick up the clocks the previous image left so a
                // silent update doesn't tell every pane it just woke.
                let mut t = pane_status::PaneStateTracker::new();
                t.read_reports(|sid| control_reader::pane_wakes::latest(sid).map(|w| w.output_at));
                let n = t.load_clocks();
                if n > 0 {
                    lx_info!(
                        "shell.pane_state.clocks_restored",
                        "carried pane idle clocks across the restart",
                        panes = n
                    );
                }
                t
            },
            pane_activity_rx,
            // Mirror of `window-state.bin` as it was on disk when this
            // process started.  Load it BEFORE any window opens: the
            // first window's own save must not be allowed to shorten
            // the list past the windows this boot is about to restore.
            saved_window_frames: marspot::state::read_windows().unwrap_or_default(),
            pane_badge_rx,
            pane_wheel_keys_rx,
            pane_tables: pane_tables::PaneTables::default(),
            pane_agent_tui_rx,
            pane_render_markup_rx,
            pane_title_rx,
            core_icons: core_icons::CoreIcons::new(),
            pane_session_begin_rx,
            pty_op_rx,
            cli_rx,
            autorun_panes: cli_socket::load_autorun(),
            autorun_last_look: None,
            op_host: SupervisorOpHost { begin_tx: pane_session_begin_tx_for_ops },
            pty_ops: plugins::pty_op::PtyOps::new(
                std::sync::Arc::new(plugins::host::HostPtyIo::new(inject_input_tx_for_ops))
                    as std::sync::Arc<dyn plugins::pty_op::PtyIo>,
            )
            .with_receipts(Some(std::sync::Arc::clone(&receipts))),
            inject_input_rx,
            active_pane_sessions: std::collections::HashMap::new(),
        }
    }
}
