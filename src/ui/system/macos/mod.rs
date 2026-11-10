//! macOS-style window chrome.  Currently: the toolbar icons.  Add
//! anything else here that's visually tied to Aqua / Sonoma
//! conventions (sheets, popovers, segmented controls when we want
//! them).

pub mod icons;

pub use icons::{GridIcon, SidebarIcon, ListTreeIcon};
