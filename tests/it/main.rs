//! The integration tests, in one binary so a change relinks one test executable instead of ten.
//! `frame_allocations` stays on its own: it installs a counting global allocator.

mod chrome_geometry;
mod display_scale_ratchet;
mod plugin_action_funnel;
mod pointer_ownership;
mod render_frame_allocations;
mod shortcuts_are_real;
mod site_claims;
mod state_guard;
mod text_layer_is_portable;
mod ui_strings;
mod ui_strings_ratchet;
mod version_identity;
