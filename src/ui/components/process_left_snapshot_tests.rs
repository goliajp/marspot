//! The process monitor with its last entry, what closed panes left
//! running, chosen: a picture at each display scale, and the two held
//! to each other.

use super::density_kit::{at_scale, capture, compare};
use super::panel_snapshot_tests::snapshot;
use crate::layout::Layout;
use crate::render_metal::MetalRenderer;
use crate::ui::components::process_panel::{self, ProcessPanelPaneRow, ProcessPanelRender, ProcessPanelRow};
use crate::ui::strings::{t, Msg};

/// The panel with three panes and the entry of what closed panes left,
/// the pointer on the kill button of one of those processes.
fn open_left(renderer: &mut MetalRenderer, layout: &Layout) {
    let pane = |name: &str, sid: u64, n_pids: u32, cpu_pct: f32, rss_mb: u64| ProcessPanelPaneRow {
        name: name.to_string(),
        sid,
        n_pids,
        cpu_pct,
        rss_kb: rss_mb * 1024,
        busy: String::new(),
    };
    let row = |depth: u8, pid: i32, comm: &str, cpu_pct: f32, rss_mb: u64| ProcessPanelRow {
        depth,
        pid,
        comm: comm.to_string(),
        cpu_pct,
        rss_kb: rss_mb * 1024,
    };
    let pane_rows = vec![
        pane("smix", 479, 10, 14.6, 682),
        pane("devops", 480, 3, 0.4, 345),
        pane("alt", 482, 2, 0.0, 69),
        pane(t(Msg::ProcessLeftGroup), 0, 4, 0.1, 96),
    ];
    let rows = vec![
        row(0, 31808, "spg · codex", 0.1, 71),
        row(1, 31822, "node", 0.0, 12),
        row(0, 52004, "sid 493 · tail", 0.0, 1),
        row(0, 60117, "torajs#2 · vite", 0.0, 12),
    ];
    let (cell_w, cell_h) = renderer.cell_dims();
    let frame = process_panel::panel_rect(layout.window_w, layout.window_h, cell_h, layout.top_inset, (0.0, 0.0));
    let l = process_panel::layout(frame, layout.top_inset, pane_rows.len(), rows.len(), 0.0, cell_w, cell_h);
    let pointer = l.kills.iter().find(|(i, _)| *i == 2).map(|(_, r)| (r.x + r.w * 0.5, r.y_top + r.h * 0.5));
    renderer.set_process_panel(Some(ProcessPanelRender {
        layout: l,
        pane_rows,
        selected_pane: 3,
        detail: t(Msg::ProcessLeftDetail).to_string(),
        rows,
        pointer,
        surface: Default::default(),
    }));
}

#[test]
fn process_panel_snapshot_left() {
    snapshot("process_left", 1600, 1000, open_left);
}

#[test]
fn process_panel_snapshot_left_2x() {
    at_scale(2.0, process_panel_snapshot_left);
}

#[test]
fn process_panel_left_holds_at_both_densities() {
    let Some(one) = capture(1.0, process_panel_snapshot_left) else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let two = capture(2.0, process_panel_snapshot_left).expect("a frame at scale 2");
    assert!(one.elems.len() > 20, "a frame of {} elements is not the panel", one.elems.len());
    if let Err(first) = compare(&one, &two, 2.0).first() {
        panic!("{first}");
    }
}

crate::snapshot_kit::snapshots_in_other_languages! {
    process_panel_snapshot_left_ja, process_panel_snapshot_left_zh = process_panel_snapshot_left;
}

#[test]
fn process_panel_snapshot_left_zh_2x() {
    at_scale(2.0, process_panel_snapshot_left_zh);
}
