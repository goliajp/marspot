//! The pane control socket's language: one JSON request per line, one
//! JSON answer per line, in order, on `<state dir>/control.sock`.
//!
//! The app's shell serves it; `marspot-peer pane …` and the MCP tools
//! speak it.  Both sides use the types here, so a field means one thing
//! everywhere.  Decoding ignores keys it does not know and refuses a
//! request missing one it needs (`bad_request`); a line longer than
//! [`MAX_LINE`] ends the connection.
//!
//! `screen` holds the one filter every place that hands a pane's text
//! to a model goes through: what a terminal shows is data, never an
//! instruction, and must not be able to fake the markup around it.

pub mod b64;
pub mod cli;
pub mod cli_helper;
pub mod cli_proc;
pub mod cli_reset;
pub mod cli_args;
mod cli_out;
pub mod cli_top;
pub mod client;
pub mod keys;
pub mod request;
pub mod response;
pub mod screen;
pub mod sha256;

pub use request::{
    AgentKind, HelperStartArgs, HelperStop, Input, Isolation, Op, OpenArgs, ProcStopArgs, ReadArgs, Request, ResetClaimArgs, RunArgs, SendArgs,
    Wait, Where,
};
pub use response::{Reject, Response};

use std::path::PathBuf;

/// The longest request line; a longer one ends the connection.
pub const MAX_LINE: usize = 256 * 1024;

/// The most bytes one `pane.send` carries.
pub const MAX_SEND_BYTES: usize = 32 * 1024;

/// The socket of the app running for this user (or for the sandbox a
/// test exported `MARSPOT_STATE_DIR` for).
pub fn socket_path() -> PathBuf {
    let root = match std::env::var_os("MARSPOT_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/marspot"),
    };
    root.join("control.sock")
}

/// What one read of a request line found.
#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    Line(String),
    /// The peer closed the connection.
    Eof,
    /// Longer than [`MAX_LINE`], or not UTF-8: the connection ends.
    Refused,
}

/// Read one `\n`-terminated line, never holding more than [`MAX_LINE`]
/// bytes of it.
pub fn read_line(r: &mut impl std::io::BufRead) -> std::io::Result<LineRead> {
    let mut buf = Vec::new();
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() { LineRead::Eof } else { LineRead::Refused });
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if buf.len() + take > MAX_LINE + 1 {
            return Ok(LineRead::Refused);
        }
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take);
        if done {
            buf.pop();
            return Ok(String::from_utf8(buf).map_or(LineRead::Refused, LineRead::Line));
        }
    }
}

macro_rules! codes {
    ($($var:ident = $s:literal),* $(,)?) => {
        /// The fixed words an answer's `error` is one of.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ErrorCode { $($var,)* }
        impl ErrorCode {
            pub const ALL: &[ErrorCode] = &[$(ErrorCode::$var,)*];
            pub fn as_str(self) -> &'static str {
                match self { $(ErrorCode::$var => $s,)* }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(ErrorCode::$var),)* _ => None }
            }
        }
    };
}

codes! {
    BadRequest = "bad_request",
    NoSuchPane = "no_such_pane",
    AmbiguousPane = "ambiguous_pane",
    LimitCount = "limit_count",
    LimitRate = "limit_rate",
    LimitBytes = "limit_bytes",
    PaneBusy = "pane_busy",
    PaneDormant = "pane_dormant",
    NotAttached = "not_attached",
    PtyQueueFull = "pty_queue_full",
    PaneNotReading = "pane_not_reading",
    NotAtShell = "not_at_shell",
    UserTyping = "user_typing",
    NotResizable = "not_resizable",
    OldSession = "old_session",
    WatchTableFull = "watch_table_full",
    CoreRestarting = "core_restarting",
    Unknown = "unknown",
    Disabled = "disabled",
    SpawnFailed = "spawn_failed",
    NoAccount = "no_account",
    NoContainerRuntime = "no_container_runtime",
    NotSupported = "not_supported",
    TooManyConnections = "too_many_connections",
    Internal = "internal",
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;

    #[test]
    fn pane_control_every_error_code_reads_back() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.as_str()), Some(*c));
        }
        assert_eq!(ErrorCode::ALL.len(), 25);
        assert_eq!(ErrorCode::parse("nope"), None);
    }

    #[test]
    fn pane_control_a_line_over_256k_ends_the_connection() {
        let mut ok = format!("{}\n", "a".repeat(MAX_LINE)).into_bytes();
        ok.extend_from_slice(b"next\n");
        let mut r = std::io::BufReader::with_capacity(4096, &ok[..]);
        assert_eq!(read_line(&mut r).unwrap(), LineRead::Line("a".repeat(MAX_LINE)));
        assert_eq!(read_line(&mut r).unwrap(), LineRead::Line("next".into()));
        assert_eq!(read_line(&mut r).unwrap(), LineRead::Eof);
        let long = format!("{}\n", "a".repeat(MAX_LINE + 1)).into_bytes();
        let mut r = std::io::BufReader::with_capacity(4096, &long[..]);
        assert_eq!(read_line(&mut r).unwrap(), LineRead::Refused);
        let mut r = std::io::BufReader::new(&b"cut short"[..]);
        assert_eq!(read_line(&mut r).unwrap(), LineRead::Refused);
    }
}
