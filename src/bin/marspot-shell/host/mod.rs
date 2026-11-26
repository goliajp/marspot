//! What the shell hosts its services with, and the tables every service
//! is checked against.

pub(crate) mod published;
// read by the platform services' debug checks and by tests until the
// services run on it
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod state_map;
