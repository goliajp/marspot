//! What a session's record has said so far, and the facts a fold of it
//! hands out.
//!
//! A fold takes the bytes appended to one record file and moves a
//! `RecordFold` forward.  It folds whole lines only: `at` is the offset
//! just past the last line it took, and the caller hands the next call
//! the bytes from `at` on, so a line split across two reads is folded
//! once it is whole.

use super::replies::Replies;
use super::short_id::ShortId;
use super::task_table::TaskTable;

/// Tokens by kind: input, cache writes kept five minutes and an hour,
/// cache reads, output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    fn fields(&self) -> [u64; 5] {
        [self.input, self.cache_5m, self.cache_1h, self.cache_read, self.output]
    }

    fn from_fields(f: [u64; 5]) -> Tokens {
        Tokens { input: f[0], cache_5m: f[1], cache_1h: f[2], cache_read: f[3], output: f[4] }
    }

    pub fn is_empty(&self) -> bool {
        self.fields().iter().all(|x| *x == 0)
    }

    pub fn add(&mut self, o: &Tokens) {
        let (a, b) = (self.fields(), o.fields());
        *self = Tokens::from_fields(std::array::from_fn(|i| a[i].saturating_add(b[i])));
    }

    /// What `self` has beyond `was`, field by field.
    pub fn beyond(&self, was: &Tokens) -> Tokens {
        let (a, b) = (self.fields(), was.fields());
        Tokens::from_fields(std::array::from_fn(|i| a[i].saturating_sub(b[i])))
    }

    /// The larger of each field.
    pub fn max(&self, o: &Tokens) -> Tokens {
        let (a, b) = (self.fields(), o.fields());
        Tokens::from_fields(std::array::from_fn(|i| a[i].max(b[i])))
    }
}

const MODEL_INLINE: usize = 64;

/// A model's name as the record gives it, held inline; a longer name is
/// cut at a character boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModelId {
    len: u8,
    bytes: [u8; MODEL_INLINE],
}

impl ModelId {
    pub fn new(name: &str) -> ModelId {
        let mut n = name.len().min(MODEL_INLINE);
        while !name.is_char_boundary(n) {
            n -= 1;
        }
        let mut m = ModelId { len: n as u8, bytes: [0; MODEL_INLINE] };
        m.bytes[..n].copy_from_slice(&name.as_bytes()[..n]);
        m
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

/// Whose turn it is in the main conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnState {
    /// Nothing read yet says.
    Unknown,
    Working,
    Ended { at: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactTrigger {
    Auto,
    Manual,
}

/// A compaction the record shows: when, what started it, and how large
/// the context was before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compacted {
    pub at: u64,
    pub trigger: CompactTrigger,
    pub pre_tokens: Option<u64>,
}

/// One record of a reply that carries its token counts.
///
/// One reply can be written as several records that repeat its id and
/// carry the counts so far; the fields below let a reader count it the
/// way it needs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub at: u64,
    /// The model that answered; None when the record names none, or
    /// names no model (an error record's placeholder).
    pub model: Option<ModelId>,
    /// This record's counts.
    pub whole: Tokens,
    /// What this record adds to earlier records of the same reply that
    /// named a model; `whole` when it names none.
    pub grew: Tokens,
    /// Cache writes as one number, before any split by lifetime.
    pub cache_written: u64,
    /// The record before it that carried counts was of the same reply.
    pub repeat: bool,
}

/// What a fold says while it reads: one call per record of a reply,
/// per compaction and per change to the session's tasks.
pub trait FoldSink {
    fn used(&mut self, r: &Reply);
    fn compacted(&mut self, c: Compacted);
    fn task(&mut self, change: TaskChange<'_>);
}

/// A change to the tasks, with what the table itself does not keep:
/// tool names and commands are text of any length, so only the sink
/// sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskChange<'a> {
    /// A tool call; `background`: it runs past the turn.
    Called { call: &'a str, tool: &'a str, background: bool, command: Option<&'a str> },
    /// The result of a call came back; `task`: the task it says it
    /// started.  `known`: the call was in the table, and `background`
    /// is the call's.
    Answered { call: &'a str, task: Option<&'a str>, known: bool, background: bool },
    /// A notification says the task ended; `waiting`: a subagent that
    /// stopped to wait on background work of its own.
    Ended { task: &'a str, at: u64, waiting: bool },
}

/// One session record's state, in fixed space.
#[derive(Clone, Debug)]
pub struct RecordFold {
    /// Offset just past the last line folded.
    pub at: u64,
    /// The bytes before the next newline are the end of a line whose
    /// start was not read.
    pub mid_record: bool,
    /// Context of the last reply of the main conversation: what it was
    /// sent and what it said.
    pub ctx_tokens: Option<u64>,
    /// The context window, when something other than the record says it.
    pub ctx_window: Option<u64>,
    /// Tokens of every reply that named a model, each counted once.
    pub spent: Tokens,
    /// The model of the reply `ctx_tokens` is of, or a later one.
    pub model: Option<ModelId>,
    pub turn: TurnState,
    /// A reply of the main conversation called a tool; None when the
    /// fold did not start at the top and has seen none.
    pub called_tool: Option<bool>,
    pub tasks: TaskTable,
    /// The reply ids `spent` was counted against.
    pub replies: Replies,
    /// The id of the last record that carried counts.
    pub last_reply: ShortId,
}

impl RecordFold {
    /// A fold of a record read from its first byte.
    pub fn from_start() -> RecordFold {
        RecordFold {
            at: 0,
            mid_record: false,
            ctx_tokens: None,
            ctx_window: None,
            spent: Tokens::default(),
            model: None,
            turn: TurnState::Unknown,
            called_tool: Some(false),
            tasks: TaskTable::default(),
            replies: Replies::default(),
            last_reply: ShortId::EMPTY,
        }
    }

    /// A fold of a record read from `at` on, where `at` may fall inside
    /// a line (a tail of the last so many bytes); the line there is
    /// skipped unless `at` is 0.
    pub fn from(at: u64) -> RecordFold {
        if at == 0 {
            return RecordFold::from_start();
        }
        RecordFold { at, mid_record: true, called_tool: None, ..RecordFold::from_start() }
    }

    /// A fold of a record read from `at` on, where `at` is known to start
    /// a line, such as the end of the file when watching began: the
    /// first line from there is folded.
    pub fn from_boundary(at: u64) -> RecordFold {
        if at == 0 {
            return RecordFold::from_start();
        }
        RecordFold { at, called_tool: None, ..RecordFold::from_start() }
    }
}
