//! The types every agent adapter and every agent service share.
//!
//! `record` is what a session's record has said so far (`RecordFold`)
//! and the facts a fold hands out on the way (`FoldSink`); the task
//! table and the reply memory it holds are fixed in size, so one fold
//! per session costs the same however long the session ran.

pub mod record;
pub mod replies;
pub mod reset;
pub mod short_id;
pub mod task_table;
pub mod turn;
pub mod wallet;

pub use record::{Compacted, CompactTrigger, FoldSink, ModelId, RecordFold, Reply, TaskChange, Tokens, TurnState};
pub use replies::Replies;
pub use short_id::ShortId;
pub use task_table::{SubLast, Task, TaskStart, TaskTable};
