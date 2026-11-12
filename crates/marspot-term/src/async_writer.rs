//! Append-only file writes that do not stall the thread that produced
//! them.
//!
//! Every byte a pane produces is written to disk twice — once raw to
//! the bytelog (so a silent update can replay it) and once encoded to
//! the scrollback (so history outlives the RAM ring).  Both are
//! product features and neither is negotiable.  What was negotiable is
//! *which thread waits for the disk*: until 2026-08-19 the parse thread
//! did, and on a bulk `cat` that put ~46 % of its samples in `write`.
//!
//! The fix is not to write less but to write elsewhere.  Throughput
//! then becomes `max(cpu, disk)` instead of `cpu + disk`, and the disk
//! side has headroom the CPU side does not: measured 630 MB/s of actual
//! writes against a corpus arriving at ~100 MB/s.
//!
//! Shape, and why this one:
//!
//! * **Double buffering, not a queue of small writes.**  The producer
//!   fills a `Vec`; when it is full the whole thing is handed to the
//!   writer thread and the producer takes an empty one from a free
//!   list.  Buffers cycle between the two threads, so a steady state
//!   allocates nothing.
//! * **Bounded, so it cannot become a memory leak.**  A fixed number of
//!   buffers may be queued or being written; when the disk falls
//!   behind, the producer waits for room and the backpressure
//!   propagates the way it always did (through the pty's own queue to
//!   the child).  A pane
//!   that writes faster than its disk forever is slower, never fatter —
//!   the "cannot get slower the longer it runs" rule.
//! * **`flush()` is a barrier, not a hint.**  Callers that are about to
//!   read the file back (cold scrollback reads, handoff before
//!   `execv`) need the bytes to be *there*, so `flush` waits for the
//!   writer thread to acknowledge everything queued ahead of it.
//! * **The file keeps up with a quiet producer.**  A partial buffer is
//!   handed over at the end of each burst ([`AsyncWriter::hand_off`]),
//!   so the file's size tracks what was produced within one burst.
//!   Readers depend on that: the size of a pane's bytelog is how L1
//!   tells a pane that is talking from one that is silent, and how a
//!   scripted op sees the first frame arrive.  Waiting for 64 KiB made
//!   a claude spinner (≈60 bytes a second) look silent for twenty
//!   minutes and a small first frame never arrive at all (2026-09-22).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long the writer leaves buffers queued in case more follow.
///
/// Every buffer written as it arrived is a thread woken, and a flood
/// hands over thousands a second: a `cat` of 134 MB woke the three
/// writers of a pane 9 000 times in half a second, and the program
/// writing into the pty -- which sleeps and is woken once a kilobyte
/// -- was a tenth slower for it.  A writer that has just written waits
/// this long for its queue to fill and writes the lot on one wake.
/// Output that has stopped does not wait: see [`AsyncWriter::hand_off`].
const LINGER: Duration = Duration::from_millis(2);

/// What is handed over at once: bytes for the file and, for a writer
/// with a side file, the bytes that go with them.
struct Buf {
    main: Vec<u8>,
    side: Vec<u8>,
}

impl Buf {
    fn sized(cap: usize) -> Self {
        Self { main: Vec::with_capacity(cap), side: Vec::new() }
    }

    fn clear(&mut self) {
        self.main.clear();
        self.side.clear();
    }

    fn is_empty(&self) -> bool {
        self.main.is_empty() && self.side.is_empty()
    }
}

enum Msg {
    /// A full buffer to append.  Returned to the free list afterwards.
    Data(Buf),
    /// Append this, then acknowledge — the barrier `flush()` waits on.
    Barrier(Buf, SyncSender<()>),
    /// Cut the file back to `len` and the side file to nothing (a
    /// clear / restart).  Goes through the queue so it cannot race the
    /// writes ahead of it.
    Truncate(u64, SyncSender<()>),
    /// From here on a buffer's side bytes are appended to this file,
    /// after its main bytes.
    Side(std::fs::File),
    /// Replace the file being written (rotation).  Anything queued
    /// ahead of it still lands in the old file, which is what makes a
    /// rotation atomic from the producer's point of view.
    Swap(std::fs::File, SyncSender<()>),
}

/// What the producer and the writer thread share.
struct Queue {
    state: Mutex<State>,
    /// The writer waits here: for work, or for more of it.
    work: Condvar,
    /// The producer waits here while `depth` buffers are in flight.
    room: Condvar,
    depth: usize,
}

#[derive(Default)]
struct State {
    queued: VecDeque<Msg>,
    /// Taken off the queue and not yet written.
    writing: usize,
    /// Something queued must not wait for company.
    now: bool,
    /// The writer is asleep with nothing to do and no deadline.
    idle: bool,
    closed: bool,
}

impl Queue {
    /// Queue `msg`, waiting for room; `now` when it must not linger.
    fn push(&self, msg: Msg, now: bool) {
        let mut st = self.state.lock().expect("the writer queue");
        while st.queued.len() + st.writing >= self.depth {
            self.work.notify_one();
            st = self.room.wait(st).expect("the writer queue");
        }
        self.put(&mut st, msg, now);
    }

    /// Queue `msg` if there is room, without waiting.
    fn try_push(&self, msg: Msg) -> Result<(), Msg> {
        let mut st = self.state.lock().expect("the writer queue");
        if st.queued.len() + st.writing >= self.depth {
            return Err(msg);
        }
        self.put(&mut st, msg, true);
        Ok(())
    }

    fn put(&self, st: &mut State, msg: Msg, now: bool) {
        st.queued.push_back(msg);
        st.now |= now;
        if st.now || st.idle || st.queued.len() + st.writing >= self.depth {
            self.work.notify_one();
        }
    }

    /// Whatever is queued is written without waiting for more.
    fn hurry(&self) {
        let mut st = self.state.lock().expect("the writer queue");
        if !st.queued.is_empty() {
            st.now = true;
            self.work.notify_one();
        }
    }

    /// The writer's side: everything queued, once it is time to write
    /// it; `None` when the producer is gone and nothing is left.
    fn take(&self) -> Option<VecDeque<Msg>> {
        let mut st = self.state.lock().expect("the writer queue");
        // it has just written: more may be a moment away
        let mut lingering = true;
        loop {
            let full = st.queued.len() >= self.depth;
            if !st.queued.is_empty() && (st.now || full || st.closed || !lingering) {
                break;
            }
            if st.queued.is_empty() && st.closed {
                return None;
            }
            if lingering {
                let (guard, waited) = self.work.wait_timeout(st, LINGER).expect("the writer queue");
                st = guard;
                lingering = !waited.timed_out();
            } else {
                st.idle = true;
                st = self.work.wait(st).expect("the writer queue");
                st.idle = false;
                // the first of a burst: see whether more follow
                lingering = true;
            }
        }
        st.now = false;
        st.writing = st.queued.len();
        Some(std::mem::take(&mut st.queued))
    }

    /// One of the taken messages is done with.
    fn wrote(&self) {
        self.state.lock().expect("the writer queue").writing -= 1;
        self.room.notify_one();
    }
}

pub struct AsyncWriter {
    queue: Arc<Queue>,
    free_rx: Receiver<Buf>,
    buf: Buf,
    /// An empty buffer a refused `hand_off` got back, kept for the next
    /// swap rather than freed — see [`AsyncWriter::hand_off`].
    spare: Option<Buf>,
    cap: usize,
    handle: Option<JoinHandle<()>>,
}

impl AsyncWriter {
    /// `cap` is the handoff size — the producer blocks only once this
    /// much has accumulated.  `depth` buffers may be in flight; total
    /// memory is bounded by `cap * (depth + 2)`, an eighth more with a
    /// side file, and a process killed without a flush loses at most
    /// `cap * (depth + 1)`.
    pub fn new(file: std::fs::File, cap: usize, depth: usize) -> Self {
        let queue = Arc::new(Queue {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            room: Condvar::new(),
            depth,
        });
        let (free_tx, free_rx) = sync_channel::<Buf>(depth + 2);
        // Seed the free list so the first few handoffs don't allocate.
        for _ in 0..2 {
            let _ = free_tx.try_send(Buf::sized(cap));
        }
        let taken = Arc::clone(&queue);
        let handle = std::thread::Builder::new()
            .name("marspot-async-writer".into())
            .spawn(move || {
                let mut file = file;
                let mut side: Option<std::fs::File> = None;
                // buffers taken together and not yet written, and who
                // waits to hear that they have been
                let mut run: Vec<Buf> = Vec::with_capacity(depth);
                let mut acks: Vec<SyncSender<()>> = Vec::new();
                while let Some(batch) = taken.take() {
                    for msg in batch {
                        let other = match msg {
                            Msg::Data(b) => {
                                run.push(b);
                                continue;
                            }
                            Msg::Barrier(b, ack) => {
                                run.push(b);
                                acks.push(ack);
                                continue;
                            }
                            other => other,
                        };
                        Self::append(&mut file, side.as_mut(), &mut run, &mut acks, &free_tx, &taken);
                        match other {
                            Msg::Truncate(len, ack) => {
                                let _ = file.set_len(len);
                                if let Some(side) = &side {
                                    let _ = side.set_len(0);
                                }
                                let _ = ack.send(());
                            }
                            Msg::Side(f) => side = Some(f),
                            Msg::Swap(new_file, ack) => {
                                file = new_file;
                                let _ = ack.send(());
                            }
                            Msg::Data(_) | Msg::Barrier(..) => unreachable!("taken above"),
                        }
                        taken.wrote();
                    }
                    Self::append(&mut file, side.as_mut(), &mut run, &mut acks, &free_tx, &taken);
                }
            })
            .expect("spawn async writer");
        Self {
            queue,
            free_rx,
            buf: Buf::sized(cap),
            spare: None,
            cap,
            handle: Some(handle),
        }
    }

    /// Append the buffers of `run` in one write to each file, hand them
    /// back and answer `acks`.
    ///
    /// One write for all of them: an append is a system call and a
    /// change to the file's size whatever it carries, and a flood that
    /// was written a buffer at a time made thousands of them a second.
    /// The main bytes go first: the side file is an index into them,
    /// and must never hold what they do not.
    fn append(
        file: &mut std::fs::File,
        side: Option<&mut std::fs::File>,
        run: &mut Vec<Buf>,
        acks: &mut Vec<SyncSender<()>>,
        free_tx: &SyncSender<Buf>,
        queue: &Queue,
    ) {
        write_all_of(file, run.iter().map(|b| b.main.as_slice()));
        if let Some(side) = side {
            write_all_of(side, run.iter().map(|b| b.side.as_slice()));
        }
        for mut b in run.drain(..) {
            b.clear();
            // Dropping the buffer when the free list is full is fine:
            // it just means the next handoff allocates.  Never block
            // here.
            let _ = free_tx.try_send(b);
            queue.wrote();
        }
        for ack in acks.drain(..) {
            let _ = ack.send(());
        }
    }

    /// Give this writer a side file: what [`AsyncWriter::write_side`]
    /// is given from here on goes to it, in the same hand-over as the
    /// main bytes written alongside, and after them on disk.
    ///
    /// For a file and the index into it.  Two writers were two threads
    /// to wake and two queues that a kill emptied at different points;
    /// one hand-over for both means the index on disk never points
    /// past the records and never lacks a record that is there.
    pub fn pair(&mut self, side: std::fs::File) {
        self.queue.push(Msg::Side(side), true);
    }

    /// Bytes for the side file, handed over with the main bytes.  A
    /// buffer holds at most an eighth of `cap` of them: an index of
    /// very short records grows as fast as the records do, and would
    /// otherwise double what a buffer may hold.
    pub fn write_side(&mut self, bytes: &[u8]) {
        self.buf.side.extend_from_slice(bytes);
        if self.buf.side.len() >= self.cap / 8 {
            self.hand_over();
        }
    }

    fn hand_over(&mut self) {
        let empty = self.take_empty();
        let full = std::mem::replace(&mut self.buf, empty);
        self.queue.push(Msg::Data(full), false);
    }

    fn take_empty(&mut self) -> Buf {
        if let Some(b) = self.spare.take() {
            return b;
        }
        match self.free_rx.try_recv() {
            Ok(mut b) => {
                b.clear();
                b
            }
            Err(_) => Buf::sized(self.cap),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.buf.main.extend_from_slice(bytes);
        if self.buf.main.len() >= self.cap {
            self.hand_over();
        }
    }

    /// Hand over what is buffered now, if the writer can take it without
    /// the producer waiting, and have everything queued written without
    /// waiting for more.  Called at the end of a burst.
    ///
    /// Never blocks and never allocates, because it runs on the parse
    /// thread after every read.  It only goes ahead when the writer has
    /// returned a buffer to swap in — which is to say, when it is not
    /// busy — and a queue that is full anyway hands the buffer back.
    /// Either refusal costs nothing: the bytes go with the next full
    /// buffer, which during the heavy output that makes the writer busy
    /// is moments away.
    pub fn hand_off(&mut self) {
        if self.buf.is_empty() {
            self.queue.hurry();
            return;
        }
        let Some(mut empty) = self.spare.take().or_else(|| self.free_rx.try_recv().ok()) else {
            self.queue.hurry();
            return;
        };
        empty.clear();
        let pending = std::mem::replace(&mut self.buf, empty);
        if let Err(Msg::Data(back)) = self.queue.try_push(Msg::Data(pending)) {
            self.spare = Some(std::mem::replace(&mut self.buf, back));
        }
    }

    /// Bytes handed over but not yet known to be on disk.  Callers that
    /// track file offsets themselves (the scrollback index) need this
    /// to reason about what a reader would see.
    pub fn pending(&self) -> usize {
        self.buf.main.len()
    }

    /// Wait until everything written so far has reached the file.
    pub fn flush(&mut self) {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        let empty = self.take_empty();
        let pending = std::mem::replace(&mut self.buf, empty);
        self.queue.push(Msg::Barrier(pending, ack_tx), true);
        let _ = ack_rx.recv();
    }

    /// Discard everything queued and cut the file to `len`.
    pub fn truncate(&mut self, len: u64) {
        // Anything still buffered belongs to the content being
        // discarded — drop it rather than writing it past the new end.
        self.buf.clear();
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        self.queue.push(Msg::Truncate(len, ack_tx), true);
        let _ = ack_rx.recv();
    }

    /// Flush, then continue appending to `new_file`.
    pub fn swap_file(&mut self, new_file: std::fs::File) {
        self.flush();
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        self.queue.push(Msg::Swap(new_file, ack_tx), true);
        let _ = ack_rx.recv();
    }
}

/// Most buffers one write carries; more are written in turns.
const AT_ONCE: usize = 8;

/// Every one of `parts` to `file`, in order, in as few writes as the
/// kernel takes them in.
fn write_all_of<'a>(file: &mut std::fs::File, parts: impl Iterator<Item = &'a [u8]>) {
    let mut held = [std::io::IoSlice::new(&[]); AT_ONCE];
    let mut n = 0;
    let mut flush = |held: &[std::io::IoSlice<'a>]| {
        let total: usize = held.iter().map(|p| p.len()).sum();
        if total == 0 {
            return;
        }
        // a short or failed vectored write leaves the rest to plain ones
        let mut written = file.write_vectored(held).unwrap_or(0);
        if written == total {
            return;
        }
        for part in held {
            let skip = written.min(part.len());
            written -= skip;
            let _ = file.write_all(&part[skip..]);
        }
    };
    for part in parts {
        held[n] = std::io::IoSlice::new(part);
        n += 1;
        if n == AT_ONCE {
            flush(&held[..n]);
            n = 0;
        }
    }
    flush(&held[..n]);
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        self.flush();
        // Closing the queue ends the writer loop; join so the file is
        // closed before we return (tests reopen these paths immediately).
        self.queue.state.lock().expect("the writer queue").closed = true;
        self.queue.work.notify_one();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("marspot-aw-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn everything_written_is_readable_after_flush() {
        let path = tmp("flush");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut w = AsyncWriter::new(f, 64, 4);
        for i in 0..1000u32 {
            w.write(&i.to_le_bytes());
        }
        w.flush();
        let mut s = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut s)
            .unwrap();
        assert_eq!(s.len(), 4000, "flush must be a barrier, not a hint");
        for i in 0..1000u32 {
            assert_eq!(&s[i as usize * 4..i as usize * 4 + 4], &i.to_le_bytes());
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Order across a rotation: bytes written before the swap belong to
    /// the old file even though the producer never waited for them.
    #[test]
    fn a_swap_does_not_reorder_across_files() {
        let a = tmp("swap-a");
        let b = tmp("swap-b");
        let fa = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&a)
            .unwrap();
        let mut w = AsyncWriter::new(fa, 1024, 4);
        w.write(b"old");
        let fb = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&b)
            .unwrap();
        w.swap_file(fb);
        w.write(b"new");
        w.flush();
        let mut sa = String::new();
        std::fs::File::open(&a)
            .unwrap()
            .read_to_string(&mut sa)
            .unwrap();
        let mut sb = String::new();
        std::fs::File::open(&b)
            .unwrap()
            .read_to_string(&mut sb)
            .unwrap();
        assert_eq!(sa, "old");
        assert_eq!(sb, "new");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// A burst smaller than the buffer reaches the file without a flush
    /// — the file's size is what tells a reader the producer spoke.
    #[test]
    fn a_small_burst_reaches_the_file_after_hand_off() {
        let path = tmp("handoff");
        let f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let mut w = AsyncWriter::new(f, 64 * 1024, 4);
        w.write(&[1u8; 700]);
        w.hand_off();
        w.write(&[2u8; 300]);
        w.hand_off();
        let mut len = 0;
        for _ in 0..200 {
            len = std::fs::metadata(&path).unwrap().len();
            if len == 1000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(len, 1000, "handed-off bytes must land without anyone flushing");
        assert_eq!(w.pending(), 0);
        drop(w);
        let _ = std::fs::remove_file(&path);
    }

    /// A writer that cannot keep up refuses the hand-off; nothing is
    /// lost or reordered, it just goes with a later buffer.
    #[test]
    fn a_refused_hand_off_loses_nothing() {
        let path = tmp("handoff-busy");
        let f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let mut w = AsyncWriter::new(f, 16, 1);
        let mut want = Vec::new();
        for i in 0..5000u32 {
            let b = i.to_le_bytes();
            w.write(&b[..(i % 4 + 1) as usize]);
            want.extend_from_slice(&b[..(i % 4 + 1) as usize]);
            w.hand_off();
        }
        w.flush();
        let mut got = Vec::new();
        std::fs::File::open(&path).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, want);
        drop(w);
        let _ = std::fs::remove_file(&path);
    }

    /// Drop is a flush too — an L3 that goes away mid-burst still
    /// leaves a complete file behind.
    #[test]
    fn drop_flushes() {
        let path = tmp("drop");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        {
            let mut w = AsyncWriter::new(f, 4096, 4);
            w.write(&[7u8; 100]);
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
        let _ = std::fs::remove_file(&path);
    }
}
