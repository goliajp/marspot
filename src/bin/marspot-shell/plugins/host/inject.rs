//! The shell's ways into a pane: the op queue's and the plugins'.

use std::sync::mpsc::Sender;

use crate::plugins::pty_op::Typed;

/// What the main loop receives on its inject_input channel.  The
/// loop walks active core's control socket and writes an InjectInput
/// frame carrying these bytes for the named session_id.
/// Reaches a pane's PTY through the main loop's inject channel.
///
/// The queue that runs operations lives in the main loop and has no
/// plugin behind it, so it needs its own way in — the same one every
/// plugin uses, rather than a private path.
pub struct HostPtyIo {
    tx: Sender<InjectInputRequest>,
}

impl HostPtyIo {
    pub fn new(tx: Sender<InjectInputRequest>) -> Self {
        Self { tx }
    }
    fn send_what(&self, session_id: u64, what: InjectWhat) -> std::io::Result<()> {
        self.tx
            .send(InjectInputRequest { session_id, what })
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "main loop dropped")
            })
    }
}

impl crate::plugins::pty_op::PtyIo for HostPtyIo {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::Input(bytes.to_vec()))
    }
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::HoldGrid(on))
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::Paste(text.to_string()))
    }
    fn program_input(&self, sid: u64, what: Typed<'_>) -> std::io::Result<()> {
        let (paste, bytes) = match what {
            Typed::Bytes(b) => (false, b.to_vec()),
            Typed::Paste(t) => (true, t.as_bytes().to_vec()),
        };
        self.send_what(sid, InjectWhat::Program { paste, bytes })
    }
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::ResetMouseReporting)
    }
}

#[derive(Debug, Clone)]
pub struct InjectInputRequest {
    pub session_id: u64,
    /// `Input` carries bytes for the PTY; `HoldGrid` carries a hold
    /// flag for the pane's L3.  One channel because they are the same
    /// journey — plugin → shell main loop → live core — and ordering
    /// between them matters: releasing a hold before the input that
    /// justifies it would show the very frames the hold is hiding.
    pub what: InjectWhat,
}

#[derive(Debug, Clone)]
pub enum InjectWhat {
    Input(Vec<u8>),
    HoldGrid(bool),
    /// Text delivered the way a paste is — see
    /// `MsgType::PaneInjectPaste`.  What anything handing a *message*
    /// to a running program wants, because only L3 knows whether that
    /// program has bracketed paste on.
    Paste(String),
    /// L1 took this pane's foreground program down; L3 should stop
    /// reporting mouse tracking as on.  See
    /// `MsgType::PaneResetMouseReporting`.
    ResetMouseReporting,
    /// Typing or a paste a program outside marspot asked for; see
    /// `MsgType::ProgramInput`.  `Input` and `Paste` are marspot's own.
    Program { paste: bool, bytes: Vec<u8> },
}

/// Concrete `InjectInputProxy` that just forwards onto the cc
/// inject-input channel.  The shell main loop drains the receiver
/// half and routes through the live core's control socket.
pub(super) struct InjectInputForwarder {
    pub(super) tx: Sender<InjectInputRequest>,
}

impl InjectInputForwarder {
    fn send(&self, session_id: u64, what: InjectWhat) -> std::io::Result<()> {
        self.tx
            .send(InjectInputRequest { session_id, what })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "inject_input channel closed",
                )
            })
    }
}

impl crate::plugins::claudecode::InjectInputProxy for InjectInputForwarder {
    fn inject_input(&self, session_id: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::Input(bytes.to_vec()))
    }

    fn hold_grid(&self, session_id: u64, on: bool) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::HoldGrid(on))
    }

    fn paste(&self, session_id: u64, text: &str) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::Paste(text.to_string()))
    }

    fn reset_mouse_reporting(&self, session_id: u64) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::ResetMouseReporting)
    }
}
