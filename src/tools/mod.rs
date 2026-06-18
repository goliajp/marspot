//! Pane-attached interactive overlays.  Each tool implements the
//! `PaneTool` trait defined in `marspot_term::render` (added in C1)
//! and is owned by a `Pane` via `pane.tools`.
//!
//! C2 introduces the search bar — pane-local input for the scrollback
//! search feature laid out across A1-B4.  The rest of the search UI
//! (result list, highlight overlay, Cmd+F intercept) lands in C3-C5.
//! Tools live under `src/` rather than `crates/marspot-term/` (per
//! `docs/scrollback-search.md` §6.4 decision) because they integrate
//! with L2's per-pane state machinery and aren't reusable in `mcli`.

pub mod search_bar;
