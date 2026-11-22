//! The integration tests, in one binary.

#[path = "../../examples/support/mod.rs"]
mod support;

mod clusters_across_update;
mod flood_frames;
mod flood_interrupt;
mod focus_reporting;
mod frame_cells;
mod history_upgrade;
mod move_region;
mod pane_awake;
mod paste_markers;
mod self_execv;
mod signals_in_the_pane;
mod stray_pointer;
