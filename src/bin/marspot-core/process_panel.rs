//! The process panel: rendering it, refreshing its rows, and killing what it lists.

use std::collections::HashMap;

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

        // Pane rows are shown busiest first, what closed panes left
        // last; `selected_pane` is a row of that order, so map it back
        // to the pane it shows.
        let mut order: Vec<usize> = (0..panel.panes.len()).collect();
        let key = |p: &PanePidTree| (p.orphans.is_some(), -p.cpu_pct);
        order.sort_by(|&a, &b| key(&panel.panes[a]).partial_cmp(&key(&panel.panes[b])).unwrap_or(std::cmp::Ordering::Equal));
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
        let mut ids = Vec::new();
        if let Some(pane) = order.get(selected_pane).and_then(|&i| panel.panes.get(i)) {
            let (sid, shell_pid) = (pane.shelld_session_id, pane.shell_child_pid.unwrap_or(0));
            detail = match pane.orphans {
                Some(_) => t(Msg::ProcessLeftDetail).to_string(),
                None => strings::text(Arg::ProcessDetail { pane: &pane.name, sid, shell_pid }),
            };
            for (label, tree) in pane_trees(pane) {
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
                    let comm = match label {
                        Some(l) if depth == 0 => l.clone(),
                        _ => node.comm.clone(),
                    };
                    rows.push(ProcessPanelRow { depth: depth.min(8) as u8, pid: node.pid, comm, cpu_pct, rss_kb });
                    ids.push(marspot::pidtree::ProcId { pid: node.pid, start_us: panel.starts.get(&node.pid).copied().unwrap_or(0) });
                }
            }
        }

        let frame = pp::panel_rect(w_phys, h_phys, cell_h, top_inset, offset);
        let layout = pp::layout(
            frame, top_inset, pane_rows.len(), rows.len(), panel.scroll_y.px(), cell_w, cell_h,
        );
        panel.scroll_y = marspot::ui::Units::from_px(layout.scroll().0);
        panel.row_ids = ids;
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

    /// F3+1.3 — send SIGTERM to the process `id` names and track it
    /// for SIGKILL escalation 2 s later if it hasn't exited.  The start
    /// time is checked again right before each signal, so a pid taken
    /// by another process since the row was listed is left alone.
    /// Logged at Info so post-mortem can correlate UI clicks with
    /// process deaths.
    pub(super) fn kill_and_track(&mut self, wi: usize, id: marspot::pidtree::ProcId) {
        match marspot::pidtree::signal(id, libc::SIGTERM) {
            Ok(()) => {
                marspot::lx_event!(
                    "PROCESS_PANEL_SIGTERM",
                    "user clicked panel [×] — SIGTERM sent",
                    pid = id.pid,
                    start_us = id.start_us
                );
                if let Some(panel) = win!(self, wi).process_panel.as_mut() {
                    panel.pending_kills.push((id, Instant::now()));
                }
            }
            Err(e) => {
                marspot::lx_warn!(
                    "process_panel.sigterm_failed",
                    &format!("{}@{}: {e}", id.pid, id.start_us)
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
        escalate(&mut panel.pending_kills, Instant::now());
    }

    /// F3+1 — re-walk libproc + read each pane's entry.toml to
    /// rebuild the process-tree cache, and read what closed panes
    /// left from the zombie watch's record.  Called when the panel
    /// opens, and on the main loop's render path when `last_refresh`
    /// is older than 2 s.  No-op when the panel is closed.
    pub(super) fn refresh_process_panel(&mut self, wi: usize) {
        let w = &mut win!(self, wi);
        let Some(panel) = w.process_panel.as_mut() else { return };
        let all = marspot::pidtree::list_all_procs();
        let now = Instant::now();
        let mut sampler = Sampler { prev: &panel.prev_pid_stats, now, cur: HashMap::new() };
        let mut panes = Vec::new();
        for (pane_idx, pane) in w.panes.iter().enumerate() {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let pid = read_shell_child_pid(sid);
            let tree = pid.and_then(|p| marspot::pidtree::tree_rooted_at(p, &all));
            let sum = sampler.sum(tree.iter());
            // Resolve the pane's name the same way the title strip
            // does: its directory.
            let name = match self.pane_cwds.get(&sid) {
                Some(p) => std::path::Path::new(p)
                    .file_name()
                    .map_or_else(|| format!("sid {sid}"), |n| n.to_string_lossy().into_owned()),
                None => format!("{}", pane_idx + 1),
            };
            panes.push(PanePidTree { shelld_session_id: sid, shell_child_pid: pid, tree, orphans: None, name, ..sum });
        }
        if let Some(left) = super::process_orphans::left(&all) {
            let sum = sampler.sum(left.iter().map(|(_, t)| t));
            let name = t(Msg::ProcessLeftGroup).to_string();
            panes.push(PanePidTree { orphans: Some(left), name, ..sum });
        }
        // Roll the stats cache forward: keep only pids we sampled this
        // tick (dead pids drop out automatically).
        let cur = sampler.cur;
        panel.panes = panes;
        panel.prev_pid_stats = cur.into_iter().map(|(pid, stat)| (pid, (stat.total_cpu_ns, now))).collect();
        panel.starts = all.iter().map(|r| (r.pid, r.start_us)).collect();
        panel.last_refresh = now;
    }
}

/// SIGKILL every process of `pending` still running under the same
/// identity past the grace period; drop the ones gone or past it.
pub(super) fn escalate(pending: &mut Vec<(marspot::pidtree::ProcId, Instant)>, now: Instant) {
    pending.retain(|&(id, sent_at)| {
        if marspot::pidtree::proc_id(id.pid) != Some(id) {
            // gone, or its pid is another process's now
            return false;
        }
        if now.duration_since(sent_at) >= KILL_ESCALATION_GRACE {
            // a failure means it went or changed hands meanwhile
            let _ = marspot::pidtree::signal(id, libc::SIGKILL);
            return false;
        }
        true
    });
}

/// The trees a pane's rows come from, each with the label of its
/// first row when that is not the command's name.
fn pane_trees(pane: &PanePidTree) -> Vec<(Option<&String>, &marspot::pidtree::ProcNode)> {
    match &pane.orphans {
        Some(left) => left.iter().map(|(label, tree)| (Some(label), tree)).collect(),
        None => pane.tree.iter().map(|tree| (None, tree)).collect(),
    }
}

/// F3+4 — samples CPU and RSS of every process a pane shows, and its
/// CPU% from the delta against the previous refresh.
struct Sampler<'a> {
    prev: &'a HashMap<i32, (u64, Instant)>,
    now: Instant,
    cur: HashMap<i32, marspot::pidtree::ProcStat>,
}

impl Sampler<'_> {
    /// The sums over `trees`, and their busiest non-shell process.
    fn sum<'t>(&mut self, trees: impl Iterator<Item = &'t marspot::pidtree::ProcNode>) -> PanePidTree {
        let (mut n, mut cpu_pct, mut rss_kb, mut top) = (0u32, 0.0f32, 0u64, (0.0f32, String::new()));
        for root in trees {
            for (_, node) in marspot::pidtree::flatten_pre_order(root) {
                let Some(stat) = marspot::pidtree::proc_stat(node.pid) else { continue };
                self.cur.insert(node.pid, stat);
                n += 1;
                rss_kb += stat.rss_bytes / 1024;
                let cpu = self.prev.get(&node.pid).map_or(0.0, |&(prev_cpu, prev_t)| {
                    stat.cpu_pct_since(prev_cpu, self.now.duration_since(prev_t).as_nanos() as u64)
                });
                cpu_pct += cpu;
                if cpu > top.0 && !node.comm.starts_with("-zsh") {
                    top = (cpu, node.comm.clone());
                }
            }
        }
        let busy = if top.0 < 0.1 { String::from("(idle)") } else { top.1 };
        PanePidTree {
            shelld_session_id: 0,
            shell_child_pid: None,
            tree: None,
            orphans: None,
            name: String::new(),
            n_pids: n,
            cpu_pct,
            rss_kb,
            busy,
        }
    }
}

#[cfg(test)]
#[path = "process_panel_tests.rs"]
mod tests;
