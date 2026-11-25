//! The process panel: rendering it, refreshing its rows, and killing what it lists.

use super::*;

impl CoreApp {
    /// Build one frame of the process monitor: its rows, and the
    /// layout the painter draws and the click handler asks.  All rects
    /// are physical pixels.
    pub(super) fn build_process_panel_render(&mut self, wi: usize) -> Option<marspot::ui::components::process_panel::ProcessPanelRender> {
        // F3+4 — char-level truncation with ASCII ellipsis (matches the
        // sidebar's `truncate_for_sidebar` style; kept inline to avoid
        // pulling a "process panel utils" module in for one helper).
        fn truncate_to(s: &str, max_chars: usize) -> String {
            let n = s.chars().count();
            if n <= max_chars { return s.to_string(); }
            let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
            format!("{head}…")
        }
        use marspot::ui::components::process_panel::{
            self as pp, ProcessPanelRender, ProcessPanelRow, ProcessPanelPaneRow,
        };
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let w = &mut win!(self, wi);
        let (w_phys, h_phys, top_inset) = (w.w_phys, w.h_phys, w.layout.top_inset);
        let offset = w.panels.offset();
        let panel = w.process_panel.as_mut()?;
        panel.selected_pane = panel.selected_pane.min(panel.panes.len().saturating_sub(1));
        let selected_pane = panel.selected_pane;

        // Pane rows are shown busiest first; `selected_pane` is a row
        // of that order, so map it back to the pane it shows.
        let mut order: Vec<usize> = (0..panel.panes.len()).collect();
        order.sort_by(|&a, &b| panel.panes[b].cpu_pct.total_cmp(&panel.panes[a].cpu_pct));
        let pane_rows: Vec<ProcessPanelPaneRow> = order
            .iter()
            .map(|&i| {
                let pane = &panel.panes[i];
                ProcessPanelPaneRow {
                    name: truncate_to(&pane.name, 22),
                    sid: pane.shelld_session_id,
                    n_pids: pane.n_pids,
                    cpu_pct: pane.cpu_pct,
                    rss_kb: pane.rss_kb,
                    busy: truncate_to(&pane.busy, 18),
                }
            })
            .collect();
        let mut detail = String::new();
        let mut rows: Vec<ProcessPanelRow> = Vec::new();
        if let Some(pane) = order.get(selected_pane).and_then(|&i| panel.panes.get(i)) {
            let (sid, shell_pid) = (pane.shelld_session_id, pane.shell_child_pid.unwrap_or(0));
            detail = strings::text(Arg::ProcessDetail { pane: &pane.name, sid, shell_pid });
            if let Some(tree) = pane.tree.as_ref() {
                for (depth, node) in marspot::pidtree::flatten_pre_order(tree) {
                    // `prev_pid_stats` holds each pid's cumulative CPU
                    // time at the last refresh; sampling again now
                    // gives the share since then.  One syscall per row.
                    let (cpu_pct, rss_kb) = match panel.prev_pid_stats.get(&node.pid) {
                        Some(&(prev, _)) => {
                            let s = marspot::pidtree::proc_stat(node.pid);
                            let dt_ns = panel.last_refresh.elapsed().as_nanos() as u64 + 1;
                            let cpu = s.map_or(0.0, |cur| cur.cpu_pct_since(prev, dt_ns));
                            (cpu, s.map_or(0, |s| s.rss_bytes / 1024))
                        }
                        None => (0.0, 0),
                    };
                    rows.push(ProcessPanelRow {
                        depth: depth.min(8) as u8,
                        pid: node.pid,
                        comm: node.comm.clone(),
                        cpu_pct,
                        rss_kb,
                    });
                }
            }
        }

        let frame = pp::panel_rect(w_phys, h_phys, cell_h, top_inset, offset);
        let layout = pp::layout(
            frame, top_inset, pane_rows.len(), rows.len(), panel.scroll_y.px(), cell_w, cell_h,
        );
        panel.scroll_y = marspot::ui::Units::from_px(layout.scroll().0);
        panel.row_pids = rows.iter().map(|r| r.pid).collect();
        panel.layout = Some(layout.clone());
        Some(ProcessPanelRender {
            layout,
            pane_rows,
            selected_pane,
            detail,
            rows,
            pointer: None,
            surface: panel.surface.clone(),
        })
    }

    /// F3+1.3 — send SIGTERM to `pid` and track it for SIGKILL
    /// escalation 2 s later if it hasn't exited.  Logged at Info so
    /// post-mortem can correlate UI clicks with process deaths.
    pub(super) fn kill_and_track(&mut self, wi: usize, pid: i32) {
        match marspot::pidtree::kill_pid(pid, libc::SIGTERM) {
            Ok(()) => {
                marspot::lx_event!(
                    "PROCESS_PANEL_SIGTERM",
                    "user clicked panel [×] — SIGTERM sent",
                    pid = pid
                );
                if let Some(panel) = win!(self, wi).process_panel.as_mut() {
                    panel.pending_kills.push((pid, Instant::now()));
                }
            }
            Err(e) => {
                marspot::lx_warn!(
                    "process_panel.sigterm_failed",
                    &format!("kill({pid}, SIGTERM): {e}")
                );
            }
        }
    }

    /// F3+1.3 — escalate any pending SIGTERM that the target ignored
    /// past the grace period.  Called from the main loop; cheap when
    /// `pending_kills` is empty (the typical case).
    pub(super) fn tick_process_panel_kills(&mut self, wi: usize) {
        let Some(panel) = win!(self, wi).process_panel.as_mut() else { return };
        if panel.pending_kills.is_empty() {
            return;
        }
        let now = Instant::now();
        panel.pending_kills.retain(|(pid, sent_at)| {
            if !marspot::pidtree::pid_is_alive(*pid) {
                // Gone already — graceful TERM took it.
                return false;
            }
            if now.duration_since(*sent_at) >= KILL_ESCALATION_GRACE {
                let _ = marspot::pidtree::kill_pid(*pid, libc::SIGKILL);
                // Drop the entry whether SIGKILL succeeded or failed;
                // a failure means the pid disappeared between the
                // is_alive check and the kill, which is a win, not
                // a loss.
                return false;
            }
            true
        });
    }

    /// F3+1 — re-walk libproc + read each pane's entry.toml to
    /// rebuild the process-tree cache.  Called when the panel opens,
    /// and on the main loop's render path when `last_refresh` is
    /// older than 2 s.  No-op when the panel is closed.
    pub(super) fn refresh_process_panel(&mut self, wi: usize) {
        // F3+4 — walk libproc, sample per-pid stats, compute CPU%
        // deltas vs the previous refresh, build per-pane aggregates.
        // Two-pass: pass A snapshots procs + new stats; pass B walks
        // panes building summaries.  prev_pid_stats is rolled forward
        // (only pids seen this tick survive into next).
        let w = &mut win!(self, wi);
        let Some(panel) = w.process_panel.as_mut() else { return };
        let all = marspot::pidtree::list_all_procs();
        let now = Instant::now();
        // Sample CPU + RSS for every pid we'll touch (descendants of
        // any pane's shell child).  Caller already has the tree so
        // we'd double-walk if we sampled lazily inside `build_node` —
        // do it once into a flat map.
        let mut cur_stats: std::collections::HashMap<i32, marspot::pidtree::ProcStat> =
            std::collections::HashMap::new();
        // Snapshot CWD for each pane's shell child for the name col.
        panel.panes.clear();
        for (pane_idx, pane) in w.panes.iter().enumerate() {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let pid = read_shell_child_pid(sid);
            let tree = pid.and_then(|p| marspot::pidtree::tree_rooted_at(p, &all));
            // Aggregate: walk descendants, sample stats, sum CPU% +
            // RSS, find top-busy non-shell process.
            let mut n = 0u32;
            let mut sum_cpu_pct = 0.0f32;
            let mut sum_rss_kb = 0u64;
            let mut top_busy = (0.0f32, String::new());
            if let Some(ref root) = tree {
                let flat = marspot::pidtree::flatten_pre_order(root);
                for (_depth, node) in &flat {
                    let p = node.pid;
                    let Some(stat) = marspot::pidtree::proc_stat(p) else { continue };
                    cur_stats.insert(p, stat);
                    n += 1;
                    sum_rss_kb += stat.rss_bytes / 1024;
                    // CPU% via delta.
                    let cpu = if let Some(&(prev_cpu, prev_t)) =
                        panel.prev_pid_stats.get(&p)
                    {
                        stat.cpu_pct_since(prev_cpu, now.duration_since(prev_t).as_nanos() as u64)
                    } else { 0.0 };
                    sum_cpu_pct += cpu;
                    if cpu > top_busy.0 && !node.comm.starts_with("-zsh") {
                        top_busy = (cpu, node.comm.clone());
                    }
                }
            }
            // Resolve the pane's name the same way the title strip
            // does: its directory.
            let name = {
                if let Some(p) = self.pane_cwds.get(&sid) {
                    std::path::Path::new(p).file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("sid {sid}"))
                } else {
                    format!("{}", pane_idx + 1)
                }
            };
            let busy = if top_busy.0 < 0.1 { String::from("(idle)") } else { top_busy.1 };
            panel.panes.push(PanePidTree {
                shelld_session_id: sid,
                shell_child_pid: pid,
                tree,
                name,
                n_pids: n,
                cpu_pct: sum_cpu_pct,
                rss_kb: sum_rss_kb,
                busy,
            });
        }
        // Roll the stats cache forward: keep only pids we sampled this
        // tick (dead pids drop out automatically).
        panel.prev_pid_stats.clear();
        for (pid, stat) in cur_stats {
            panel.prev_pid_stats.insert(pid, (stat.total_cpu_ns, now));
        }
        panel.last_refresh = now;
    }
}
