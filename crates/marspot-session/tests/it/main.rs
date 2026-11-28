//! The integration tests, in one binary.

#[path = "../../examples/support/mod.rs"]
mod support;

mod build_sha_without_repo;
mod clusters_across_update;
mod close_dir;
mod control_fd_across_update;
mod exec_contract_across_update;
mod flood_frames;
mod flood_interrupt;
mod focus_reporting;
mod frame_cells;
mod helper_container;
mod history_upgrade;
mod install_adoption_check;
mod move_region;
mod pane_awake;
mod pane_link;
mod paste_markers;
mod prompt_jump;
mod self_execv;
mod shadow_levels;
mod shadow_session;
mod signals_in_the_pane;
mod stray_pointer;
mod typing;
mod typing_after_exec;
mod typing_run_end;
mod typing_run_trailer;
mod typing_run_unfinished;
mod watch;
mod watch_prompt;
mod watch_swap;
mod watch_text;
