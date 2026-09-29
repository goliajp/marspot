//! macOS-style window chrome.  Currently: traffic lights + title bar
//! that composes them.  Add anything else here that's visually tied
//! to Aqua / Sonoma conventions (sheets, popovers, segmented controls
//! when we want them).

pub mod traffic_lights;
pub mod title_bar;
pub mod icons;

pub use traffic_lights::{TrafficLights, TrafficLightHit};
pub use title_bar::TitleBar;
pub use icons::{GridIcon, SidebarIcon, ListTreeIcon};
