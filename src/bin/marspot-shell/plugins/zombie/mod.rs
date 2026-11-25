//! The zombie watch: finds processes an agent left behind that almost
//! certainly nobody uses any more, and tells the pane they belong to.
//! It never ends them itself.
//!
//! So far only which pane a process belongs to (`attrib`).

// nothing calls it until the watch's checks land
#[allow(dead_code)]
pub(crate) mod attrib;
