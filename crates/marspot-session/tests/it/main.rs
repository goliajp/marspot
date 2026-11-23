//! The integration tests, in one binary.

#[path = "../../examples/support/mod.rs"]
mod support;

mod clusters_across_update;
mod close_dir;
mod flood_frames;
mod flood_interrupt;
mod focus_reporting;
mod frame_cells;
mod helper_container;
mod history_upgrade;
mod move_region;
mod pane_awake;
mod pane_link;
mod paste_markers;
mod self_execv;
mod shadow_session;
mod signals_in_the_pane;
mod stray_pointer;
mod typing;
mod typing_after_exec;
mod typing_run_trailer;
mod watch;
mod watch_prompt;
mod watch_swap;
mod watch_text;
