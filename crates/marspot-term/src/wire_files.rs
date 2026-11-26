//! Formats of files under the state root that one build writes and
//! another build reads: the writer and every reader link this module,
//! so a field cannot drift between images.

pub mod fact_line;
