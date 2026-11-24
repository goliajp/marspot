//! How many bytes the pane's PTY has produced, and whether a text a
//! text watch waits for can have reached the grid in them.

use super::LocalSession;
use prefilter::Prefilter;

#[path = "prefilter.rs"]
mod prefilter;

/// Every byte taken in, held or parsed, counted once: the bytes the
/// bytelog is given.
#[derive(Default)]
pub(crate) struct Output {
    pos: u64,
    texts: Prefilter,
}

impl Output {
    pub(super) fn resumed(pos: u64) -> Self {
        Self { pos, ..Self::default() }
    }

    /// Count `bytes`, and read them for the texts while any is set.
    #[inline]
    pub(super) fn took(&mut self, bytes: &[u8]) {
        self.pos += bytes.len() as u64;
        if self.texts.active() {
            self.texts.took(bytes);
        }
    }
}

pub(super) fn has_byte(bytes: &[u8], b: u8) -> bool {
    find_byte(bytes, b).is_some()
}

pub(super) fn find_byte(bytes: &[u8], b: u8) -> Option<usize> {
    // SAFETY: reads `bytes.len()` bytes of a live slice
    let at = unsafe { libc::memchr(bytes.as_ptr().cast(), libc::c_int::from(b), bytes.len()) };
    (!at.is_null()).then(|| at.addr() - bytes.as_ptr().addr())
}

impl LocalSession {
    /// Bytes the PTY has produced: the output position.
    pub(crate) fn output_pos(&self) -> u64 {
        self.out.pos
    }

    /// Look for these texts, each with an id under 8, in what is taken
    /// in from now on; none stops looking.
    pub(crate) fn look_for_texts(&mut self, texts: &[(usize, &[u8])]) {
        if !self.out.texts.active() {
            // held output went past the prefilter but not into the grid
            self.out.texts.starts_on(prefilter::screen_after(self.terminal.in_alt_screen(), &self.held));
        }
        self.out.texts.set(texts);
    }

    /// Whether what was taken in since the last ask can have put one of
    /// the texts on the grid, and a bit for the id of each text that
    /// was in it whole.
    pub(crate) fn take_text_check(&mut self) -> (bool, u8) {
        self.out.texts.take(self.terminal.in_alt_screen())
    }
}

/// A session that only parses: the test queues its output.
#[cfg(test)]
impl LocalSession {
    pub(crate) fn parsing_only(cols: u16, rows: u16) -> (Self, std::sync::mpsc::Sender<crate::pty_reader::Chunk>) {
        use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
        use std::sync::Arc;
        let pty = Pty::spawn(PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            size: TerminalSize { cols, rows, pixel_width: 0, pixel_height: 0 },
            ..Default::default()
        })
        .expect("spawn /bin/sh");
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Self {
            id: 1,
            pty: Arc::new(pty),
            child_pid: 0,
            terminal: marspot_term::terminal::Terminal::new(cols, rows),
            bytelog: None,
            out: Output::default(),
            rx,
            reader: None,
            writer: super::PtyWriter::new("l3-pty-writer-t", std::io::sink(), marspot_term::frame_writer::cap::PTY),
            replies: Default::default(),
            line: Default::default(),
            exited: Default::default(),
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            hold: false,
            held: Vec::new(),
            held_bytes: 0,
            unfinished: None,
            before_key: 0,
            cols,
            rows,
        };
        (session, tx)
    }
}
