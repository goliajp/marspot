//! Shared machinery for the L3 probes.
//!
//! Not an example itself: cargo only picks up `examples/*/main.rs`, so a
//! module directory beside them is ignored. Each probe pulls it in with
//! `#[path = "support/mod.rs"] mod support;`.
//!
//! Seven probes lost their source in the June history rebuild and were
//! rewritten 2026-10-01. The spawn sequence is the thing they all need to
//! get right -- an L3 is started with a shm region and a control socket it
//! inherits, and a sample taken before it publishes a frame is a sample of
//! a process still allocating its grid.

#![allow(dead_code)]

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{GridShmReader, create_region};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::shell_proto::{
    FIRST_WINDOW_ID, Frame, MsgType, PROTO_VERSION, decode_hello_ack, encode_hello,
    encode_key_event, event_to_wire,
};

pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;

pub fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

/// Clear FD_CLOEXEC so an inherited fd survives the child's exec.  L2
/// dup2's onto fixed fds in pre_exec; a probe passes the real fd numbers
/// instead, which marspot-session parses from its env.
pub fn clear_cloexec(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        die(format!(
            "fcntl CLOEXEC on fd {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
}

/// One live session: the child, the control socket's parent end, and a
/// reader on the region it publishes into.
pub struct Session {
    pub child: Child,
    pub control: UnixStream,
    pub reader: GridShmReader,
}

impl Session {
    pub fn spawn(bin: &Path, cols: u16, rows: u16) -> Self {
        Self::spawn_with_id(bin, cols, rows, None)
    }

    /// Spawn on a named session id, which is what a replacement does: the
    /// id is how it finds the bytelog to replay and so how a swap keeps
    /// the pane's history across the handover.
    pub fn spawn_with_id(bin: &Path, cols: u16, rows: u16, id: Option<u64>) -> Self {
        let region = create_region(cols, rows)
            .unwrap_or_else(|e| die(format!("create_region: {e}")));
        clear_cloexec(region.as_raw_fd());
        let (parent, child_end) =
            UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
        clear_cloexec(child_end.as_raw_fd());
        let mut cmd = Command::new(bin);
        cmd.env("MARSPOT_SHELL_CONTROL_FD", child_end.as_raw_fd().to_string())
            .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string());
        if let Some(id) = id {
            cmd.env("MARSPOT_SESSION_ID", id.to_string());
        }
        let child = cmd
            .spawn()
            .unwrap_or_else(|e| die(format!("spawn {}: {e}", bin.display())));
        drop(child_end);
        let reader = GridShmReader::from_fd(region.as_raw_fd())
            .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));
        Self {
            child,
            control: parent,
            reader,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Settled means it published a frame and its shell has had time to
    /// print a prompt.
    pub fn wait_ready(&self, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.reader.seq() == 0 {
            if Instant::now() >= deadline {
                die(format!("{what}: never published a frame"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Block until the publish seq moves past `from`.
    pub fn wait_seq_past(&self, from: u64, what: &str) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.reader.seq();
            if s != from && s != 0 {
                return s;
            }
            if Instant::now() >= deadline {
                die(format!("timed out waiting for {what} (seq stuck at {from})"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn send_char(&self, c: char) {
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char(c),
            text: Some(c.to_string()),
            ..Default::default()
        };
        let frame = Frame::new(
            MsgType::KeyEvent,
            encode_key_event(&event_to_wire(&ev, Modifiers::default()), FIRST_WINDOW_ID),
        );
        let mut w = &self.control;
        frame
            .write_to(&mut w)
            .unwrap_or_else(|e| die(format!("write key frame: {e}")));
        w.flush().ok();
    }

    /// Send any frame on the control socket.
    pub fn send_frame(&self, msg_type: MsgType, payload: Vec<u8>) {
        let frame = Frame::new(msg_type, payload);
        let mut w = &self.control;
        frame
            .write_to(&mut w)
            .unwrap_or_else(|e| die(format!("write {msg_type:?}: {e}")));
        w.flush().ok();
    }

    /// Wait for a frame of `want` on the control socket, skipping the
    /// others -- L3 talks for its own reasons, so the reply is not
    /// necessarily the next thing to arrive.
    pub fn await_frame(&self, want: MsgType, secs: u64) -> Frame {
        self.control
            .set_read_timeout(Some(Duration::from_millis(250)))
            .ok();
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut r = &self.control;
        loop {
            match Frame::read_from(&mut r) {
                Ok(Some(f)) if f.msg_type == want => return f,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => die(format!("reading for {want:?}: {e}")),
            }
            if Instant::now() >= deadline {
                die(format!("no {want:?} within {secs}s"));
            }
        }
    }

    /// Type a line and press Enter.  The shell sees a command.
    pub fn send_line(&self, line: &str) {
        for c in line.chars() {
            self.send_char(c);
        }
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Named(NamedKey::Enter),
            text: Some("\r".to_string()),
            ..Default::default()
        };
        let frame = Frame::new(
            MsgType::KeyEvent,
            encode_key_event(&event_to_wire(&ev, Modifiers::default()), FIRST_WINDOW_ID),
        );
        let mut w = &self.control;
        frame
            .write_to(&mut w)
            .unwrap_or_else(|e| die(format!("write enter frame: {e}")));
        w.flush().ok();
    }

    /// The character at `(col, row)` of the most recent published frame.
    pub fn cell(&self, col: u16, row: u16, cols: u16) -> char {
        let mut buf = Vec::new();
        let _ = self
            .reader
            .read(&mut buf, &mut Vec::new())
            .unwrap_or_else(|| die("no published frame to read"));
        let idx = row as usize * cols as usize + col as usize;
        buf.get(idx).map(|c| c.ch).unwrap_or('\0')
    }

    /// The whole published grid as trimmed lines, read once.
    ///
    /// Reading per cell meant a full grid copy per character -- 1920 of
    /// them for one screenful, which turned a probe that polls the screen
    /// into one that never finishes.
    pub fn screen(&self, cols: u16, rows: u16) -> Vec<String> {
        self.try_screen(cols, rows)
            .unwrap_or_else(|| die("no published frame to read"))
    }

    /// The screen, or `None` when there is no frame to read yet.
    ///
    /// A wait loop needs this one: dying because a single read came up
    /// empty turns "not published yet" into a failure, which is what made
    /// the selection probe flake inside the suite while passing alone.
    pub fn try_screen(&self, cols: u16, rows: u16) -> Option<Vec<String>> {
        let mut buf = Vec::new();
        self.reader.read(&mut buf, &mut Vec::new())?;
        Some(
            (0..rows)
                .map(|r| {
                    let start = r as usize * cols as usize;
                    buf.get(start..start + cols as usize)
                        .map(|row| row.iter().map(|c| c.ch).collect::<String>())
                        .unwrap_or_default()
                        .trim_end()
                        .to_string()
                })
                .collect(),
        )
    }

    pub fn cursor(&self) -> (u16, u16) {
        let mut buf = Vec::new();
        let f = self
            .reader
            .read(&mut buf, &mut Vec::new())
            .unwrap_or_else(|| die("no published frame to read"));
        (f.cursor_col, f.cursor_row)
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resident size of one pid in KiB, or `None` once it is gone.
pub fn rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Kill and reap every session.  A session that outlives its probe is a
/// leaked pane process, and 176 of those once came from not doing this.
pub fn cleanup(sessions: &mut Vec<Session>) {
    for s in sessions.iter_mut() {
        s.kill();
    }
    sessions.clear();
}

/// Write one character as a KeyEvent on any control stream.
///
/// After a self-execv the inherited fd is gone and L2 reconnects over the
/// session's listener, so a probe that wants to type afterwards needs this
/// rather than `Session::send_char`.
pub fn send_char_on(stream: &UnixStream, c: char) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char(c),
        text: Some(c.to_string()),
        ..Default::default()
    };
    let frame = Frame::new(
        MsgType::KeyEvent,
        encode_key_event(&event_to_wire(&ev, Modifiers::default()), FIRST_WINDOW_ID),
    );
    let mut w = stream;
    frame
        .write_to(&mut w)
        .unwrap_or_else(|e| die(format!("write key frame on reattach: {e}")));
    w.flush().ok();
}

/// Connect to a session's listener and complete the handshake, the way L2
/// reattaches after the session has execv'd itself.
///
/// The handshake is not optional: the accept path reads a `Hello` carrying
/// the protocol version and answers `HelloAck` before handing the stream
/// to the session's main loop.  A connection that skips it is accepted at
/// the socket level and then ignored -- which from outside looks exactly
/// like a session that has stopped taking input.
pub fn reattach(sock: &Path, secs: u64) -> Option<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(stream) = UnixStream::connect(sock) {
            stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
            let hello = Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION));
            let mut w = &stream;
            if hello.write_to(&mut w).is_ok() {
                w.flush().ok();
                let mut r = &stream;
                if let Ok(Some(f)) = Frame::read_from(&mut r)
                    && f.msg_type == MsgType::HelloAck
                    && decode_hello_ack(&f.payload).is_ok()
                {
                    stream.set_read_timeout(None).ok();
                    return Some(stream);
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
