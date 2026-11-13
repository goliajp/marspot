//! Parsing what the reader has queued.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{HOLD_CAP, LocalSession};
use crate::publish_gate::Follow;
use crate::pty_reader::Chunk;

/// Most that is parsed between two looks at whether a key has arrived:
/// some 40 to 70 microseconds of parsing.
const SLICE_BYTES: usize = 16 * 1024;

/// How far back from its full length a slice may end, to end on a line.
const SLICE_SLACK: usize = 1024;

/// Where the slice of `bytes` that starts at `from` ends: after a line
/// feed if one of its last bytes is one, since the parser takes a
/// whole line faster than two halves of it, and otherwise not in the
/// middle of a character.
fn slice_end(bytes: &[u8], from: usize, slice: usize) -> usize {
    let end = from.saturating_add(slice);
    if end >= bytes.len() {
        return bytes.len();
    }
    let earliest = end.saturating_sub(SLICE_SLACK).max(from + 1);
    if let Some(feed) = bytes[earliest..end].iter().rposition(|&b| b == b'\n') {
        return earliest + feed + 1;
    }
    // a UTF-8 continuation byte belongs with the bytes before it
    (earliest..=end).rev().find(|&at| bytes[at] & 0xC0 != 0x80).unwrap_or(end)
}

/// When a bounded pump gives the loop its turn back.
pub(crate) struct Pace<'a> {
    /// Longest it parses.
    pub(crate) budget: Duration,
    /// A key's answer is awaited: stop after each chunk, or each
    /// slice, that may hold it, so that a frame can follow.
    pub(crate) follow: Option<Follow>,
    /// Raised when a key has arrived: stop at the next slice.
    pub(crate) key_waiting: &'a AtomicBool,
}

impl LocalSession {
    /// Drain every chunk the reader has queued, append each to the
    /// bytelog, feed it into the terminal parser.  Returns bytes
    /// consumed this tick (0 = nothing happened).
    pub fn pump(&mut self) -> usize {
        self.drain(None).0
    }

    /// [`Self::pump`], stopping where `pace` says.  The flag says
    /// output is still queued.
    ///
    /// The reader queues whatever the PTY gives it, and a program can
    /// write faster than the parser reads -- `cat` of a big file, `yes`.
    /// Draining to empty then never ends while the program runs, and
    /// the loop that called it stops publishing frames, applying
    /// resizes and delivering keys, Ctrl-C included.
    pub(crate) fn pump_within(&mut self, pace: Pace) -> (usize, bool) {
        self.drain(Some(pace))
    }

    /// A key is on its way to the pane's program.  What the reader has
    /// already handed over was read before the key was written, and so
    /// was what it has in hand and hands over next: none of it can hold
    /// the program's answer.
    pub(crate) fn typed(&mut self) {
        self.before_key = self.reader.as_ref().map_or(0, |r| r.in_flight()) + 1;
    }

    fn drain(&mut self, pace: Option<Pace>) -> (usize, bool) {
        let deadline = pace.as_ref().map(|p| Instant::now() + p.budget);
        let key_waiting = || pace.as_ref().is_some_and(|p| p.key_waiting.load(Ordering::Acquire));
        let mut total = 0;
        let mut more = false;
        let follow = pace.as_ref().and_then(|p| p.follow);
        // bytes parsed in this pump that may hold a key's answer
        let mut fresh = 0;
        // enough of them, by what `follow` asks, for a frame
        let mut answered = false;
        let raw = self.line.output_is_raw(self.pty.raw_master(), self.shell_pgrp());
        loop {
            let (chunk, mut at) = match self.unfinished.take() {
                Some(unfinished) => unfinished,
                None => match self.rx.try_recv() {
                    Ok(chunk) => (chunk, 0),
                    Err(_) => break,
                },
            };
            let stop = || (follow.is_some() && answered) || key_waiting() || deadline.is_some_and(|d| Instant::now() >= d);
            if total > 0 && pace.is_some() && stop() {
                self.unfinished = Some((chunk, at));
                more = true;
                break;
            }
            // see `line_discipline`: false unless a repair is pending
            let converted = raw.then(|| crate::line_discipline::with_carriage_returns(chunk.bytes()[at..].to_vec()));
            let bytes = converted.as_deref().unwrap_or(&chunk.bytes()[at..]);
            // held or converted output goes in one piece
            let slice = if self.hold || raw { bytes.len() } else { SLICE_BYTES };
            let mut done = 0;
            while done < bytes.len() {
                let piece = &bytes[done..slice_end(bytes, done, slice)];
                self.take_in(piece);
                done += piece.len();
                total += piece.len();
                if self.before_key == 0 {
                    fresh += piece.len();
                }
                let a_slice = follow == Some(Follow::Slice) && fresh + SLICE_SLACK >= SLICE_BYTES;
                if done < bytes.len() && (a_slice || key_waiting()) {
                    break;
                }
            }
            if done < bytes.len() {
                // only an unconverted chunk is cut: `done` counts its own bytes
                at += done;
                self.unfinished = Some((chunk, at));
                more = true;
                break;
            }
            match self.before_key {
                0 => answered = follow == Some(Follow::Chunk) || fresh + SLICE_SLACK >= SLICE_BYTES,
                _ => self.before_key -= 1,
            }
            if let Some(r) = self.reader.as_ref() {
                r.recycle(chunk);
            }
        }
        if total > 0 {
            self.last_output = Some(Instant::now());
        }
        // The bytelog's size is what L1 reads to tell whether this pane
        // is talking; a burst must not wait in a buffer for 64 KiB of
        // company.  A flood fills buffers by itself, until it ends.
        if let Some(b) = self.bytelog.as_mut().filter(|_| !more && total < crate::publish_gate::FloodGate::BULK_BYTES) {
            b.hand_off();
        }
        self.flush_capability_responses();
        // see `reply_guard`: on every pump while replies are unaccounted
        // for -- any output, the shell's prompt included, or the loop's
        // short wake while watching
        if self.replies.is_watching() {
            let delivered = self.writer.backlog() == 0;
            self.replies.check_and_log(self.pty.raw_master(), self.shell_pgrp(), delivered, self.id);
        }
        // see `line_discipline`: a job starts drawing, or the prompt is back
        if total > 0 || self.line.wants_look() {
            self.line.look(self.pty.raw_master(), self.shell_pgrp(), self.id, self.writer.backlog() == 0);
        }
        (total, more)
    }

    /// Log `bytes` and parse them, or keep them while the picture is held.
    fn take_in(&mut self, bytes: &[u8]) {
        // The bytelog is written whether or not the picture is
        // held: everything that reads it must see the pane talk.
        if let Some(b) = self.bytelog.as_mut() {
            let _ = b.append(bytes);
        }
        if self.hold {
            self.held_bytes += bytes.len();
            self.held.push(bytes.to_vec());
            if self.held_bytes >= HOLD_CAP {
                marspot_term::lx_warn!(
                    "l3.hold.cap_reached",
                    "held output hit the cap; releasing the freeze \
                     mid-paint — the pane will visibly jump",
                    held_bytes = self.held_bytes as u64,
                    cap = HOLD_CAP as u64
                );
                self.hold_grid(false);
            }
        } else {
            self.terminal.feed(bytes);
        }
    }
}

/// A chunk the pump stopped in the middle of, and how far it got.
pub(super) type Unfinished = (Chunk, usize);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Sender};

    use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
    use marspot_term::terminal::Terminal;

    const HOUR: Duration = Duration::from_secs(3600);

    /// A session that parses what the test queues; its pty is not read.
    fn session() -> (LocalSession, Sender<Chunk>) {
        let pty = Pty::spawn(PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 3".into()],
            size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            argv0: None,
            cwd: None,
            ..Default::default()
        })
        .expect("spawn /bin/sh");
        let (tx, rx) = mpsc::channel();
        let session = LocalSession {
            id: 1,
            pty: Arc::new(pty),
            child_pid: 0,
            terminal: Terminal::new(80, 24),
            bytelog: None,
            rx,
            reader: None,
            writer: super::super::PtyWriter::new("l3-pty-writer-1", std::io::sink(), marspot_term::frame_writer::cap::PTY),
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
            cols: 80,
            rows: 24,
        };
        (session, tx)
    }

    /// Numbered lines, so that a piece parsed twice or not at all shows.
    fn lines(from: usize, bytes: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes + 32);
        let mut n = from;
        while out.len() < bytes {
            out.extend_from_slice(format!("line {n:07} \x1b[1m漢字\x1b[0m\r\n").as_bytes());
            n += 1;
        }
        out
    }

    fn screen(t: &Terminal) -> String {
        let g = t.grid();
        (0..g.rows()).flat_map(|row| (0..g.cols()).map(move |col| (col, row))).map(|(col, row)| g.cell(col, row).ch).collect()
    }

    /// A slice ends on a line, a little short of its full length.
    fn a_slice(bytes: usize) -> bool {
        bytes <= SLICE_BYTES && bytes + SLICE_SLACK > SLICE_BYTES
    }

    /// A slice ends after a line feed when one is near its end, and
    /// never inside a character.
    #[test]
    fn a_slice_ends_on_a_line_or_at_least_between_two_characters() {
        let text = lines(0, 3 * SLICE_BYTES);
        let end = slice_end(&text, 0, SLICE_BYTES);
        assert!(a_slice(end));
        assert_eq!(text[end - 1], b'\n');
        assert_eq!(slice_end(&text, end, usize::MAX), text.len(), "what is left goes in one piece");

        let unbroken = "漢".repeat(SLICE_BYTES).into_bytes();
        let end = slice_end(&unbroken, 1, SLICE_BYTES);
        assert!(end > 1 && end <= 1 + SLICE_BYTES);
        assert_eq!(end % 3, 0, "between two three-byte characters");
        assert!(std::str::from_utf8(&unbroken[end..]).is_ok());
    }

    fn pace(follow: Option<Follow>, key_waiting: &AtomicBool) -> Pace<'_> {
        Pace { budget: HOUR, follow, key_waiting }
    }

    /// A key that arrives while a chunk is being parsed gets the loop
    /// back within one slice, and the rest of the chunk is parsed
    /// afterwards, once and in order.
    #[test]
    fn a_key_ends_the_pump_within_a_slice_and_nothing_is_lost() {
        let (mut s, tx) = session();
        let chunk = lines(0, 4 * SLICE_BYTES + 100);
        let behind = lines(100_000, SLICE_BYTES);
        tx.send(chunk.clone().into()).expect("queue a chunk");
        tx.send(behind.clone().into()).expect("queue a chunk");
        let key = AtomicBool::new(true);

        let (first, more) = s.pump_within(pace(None, &key));
        assert!(a_slice(first), "one slice, however much is queued: {first}");
        assert!(more, "the rest of the chunk is still to parse");
        let (second, more) = s.pump_within(pace(None, &key));
        assert!(a_slice(second), "a key that is still waiting ends every pump as soon: {second}");
        assert!(more);

        key.store(false, Ordering::Release);
        let (rest, more) = s.pump_within(pace(None, &key));
        assert_eq!(first + second + rest, chunk.len() + behind.len());
        assert!(!more);

        let mut whole = Terminal::new(80, 24);
        whole.feed(&chunk);
        whole.feed(&behind);
        assert_eq!(screen(s.terminal()), screen(&whole), "parsed in pieces as it would have been in one");
    }

    /// No key waiting: a pump with time left parses all there is.
    #[test]
    fn a_pump_nothing_interrupts_parses_all_that_is_queued() {
        let (mut s, tx) = session();
        let chunk = lines(0, 4 * SLICE_BYTES + 100);
        for _ in 0..3 {
            tx.send(chunk.clone().into()).expect("queue a chunk");
        }
        let (n, more) = s.pump_within(pace(None, &AtomicBool::new(false)));
        assert_eq!(n, 3 * chunk.len());
        assert!(!more);
    }

    /// While a key's answer is awaited the pump stops after each chunk
    /// that may hold it -- the loop publishes there -- and runs through
    /// the chunks read before the key, which cannot.
    #[test]
    fn a_pump_awaiting_an_answer_stops_after_each_chunk_that_may_hold_it() {
        let (mut s, tx) = session();
        let chunk = lines(0, SLICE_BYTES + 100);
        for _ in 0..5 {
            tx.send(chunk.clone().into()).expect("queue a chunk");
        }
        let no_key = AtomicBool::new(false);
        // what `typed` counts when the reader has two chunks in flight
        s.before_key = 2;

        let (n, more) = s.pump_within(pace(Some(Follow::Chunk), &no_key));
        assert_eq!(n, 3 * chunk.len(), "the two from before the key and the first after it");
        assert!(more);
        let (n, more) = s.pump_within(pace(Some(Follow::Chunk), &no_key));
        assert_eq!(n, chunk.len(), "then one at a time");
        assert!(more);
        let (n, more) = s.pump_within(pace(Some(Follow::Chunk), &no_key));
        assert_eq!(n, chunk.len());
        assert!(!more, "the queue was looked at: nothing is behind the last chunk");
    }

    /// Right after a key the pump stops after each slice that may hold
    /// the answer, still running through what was read before the key.
    #[test]
    fn a_pump_right_after_a_key_stops_after_each_slice_that_may_hold_the_answer() {
        let (mut s, tx) = session();
        let chunk = lines(0, 2 * SLICE_BYTES + 100);
        for _ in 0..2 {
            tx.send(chunk.clone().into()).expect("queue a chunk");
        }
        let no_key = AtomicBool::new(false);
        s.before_key = 1;

        let slice = Some(Follow::Slice);
        let (first, more) = s.pump_within(pace(slice, &no_key));
        assert!(a_slice(first - chunk.len()), "the chunk from before the key and one slice: {first}");
        assert!(more);
        let (second, more) = s.pump_within(pace(slice, &no_key));
        assert!(a_slice(second), "{second}");
        assert!(more);
        let (last, more) = s.pump_within(pace(slice, &no_key));
        assert_eq!(first + second + last, 2 * chunk.len(), "the end of the chunk");
        assert!(!more);

        // a kilobyte at a time, as a reader hands it over right after a
        // key: no frame until a slice's worth has been parsed
        let piece = lines(0, 1024);
        let pieces = SLICE_BYTES / piece.len() + 3;
        for _ in 0..pieces {
            tx.send(piece.clone().into()).expect("queue a chunk");
        }
        let (n, more) = s.pump_within(pace(slice, &no_key));
        assert!(n + SLICE_SLACK >= SLICE_BYTES && n < SLICE_BYTES + piece.len(), "{n}");
        assert!(more);
        let (rest, more) = s.pump_within(pace(slice, &no_key));
        assert_eq!(n + rest, pieces * piece.len());
        assert!(!more);
    }
}
