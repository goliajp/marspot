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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

mod files;
mod queue;
pub use files::{delete_with, doomed_path};
use files::{rotated, write_all_of};
use queue::{Buf, Msg, Queue};

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
        let queue = Arc::new(Queue::new(depth));
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
                            Msg::Rotate { live, retired } => {
                                if let Some(fresh) = rotated(&live, &retired) {
                                    file = fresh;
                                }
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
        // one comparison for the write that stays inside the buffer:
        // the scrollback makes one of these for every line
        if self.buf.main.len() + bytes.len() >= self.cap {
            return self.write_to_the_brim(bytes);
        }
        self.buf.main.extend_from_slice(bytes);
    }

    /// A write that fills the buffer or does not fit in it.
    ///
    /// What does not fit goes into the next buffer.  Appended to this
    /// one it grew it past `cap`, for good: fed 16 KiB at a time a
    /// bytelog handed over 64 to 80 KiB, and each of its buffers ended
    /// up twice the size it was made with.
    #[cold]
    fn write_to_the_brim(&mut self, bytes: &[u8]) {
        if !self.buf.main.is_empty() && self.buf.main.len() + bytes.len() > self.cap {
            self.hand_over();
        }
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

    /// Retire the file being written, which is at `live`, to `retired`
    /// and go on in a fresh file at `live`, without waiting for either.
    ///
    /// What was written before this call ends up in the retired file
    /// and what is written after it in the fresh one; a reader that
    /// wants to see that calls [`Self::flush`] first.  When the
    /// renaming fails the old file goes on being written.
    pub fn rotate(&mut self, live: PathBuf, retired: PathBuf) {
        if !self.buf.is_empty() {
            self.hand_over();
        }
        self.queue.push(Msg::Rotate { live, retired }, true);
    }

    /// Flush, then continue appending to `new_file`.
    pub fn swap_file(&mut self, new_file: std::fs::File) {
        self.flush();
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        self.queue.push(Msg::Swap(new_file, ack_tx), true);
        let _ = ack_rx.recv();
    }
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        self.flush();
        // Closing the queue ends the writer loop; join so the file is
        // closed before we return (tests reopen these paths immediately).
        self.queue.close();
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

    /// A write that would take a buffer past its size starts the next
    /// one: no buffer is ever grown by writes smaller than it.
    #[test]
    fn a_write_that_does_not_fit_goes_into_the_next_buffer() {
        let path = tmp("fit");
        let f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let mut w = AsyncWriter::new(f, 64, 4);
        w.write(&[1u8; 40]);
        w.write(&[2u8; 40]);
        assert_eq!(w.pending(), 40, "the second write was appended to a buffer it did not fit in");
        assert!(w.buf.main.capacity() <= 64, "the buffer grew to {}", w.buf.main.capacity());
        w.write(&[3u8; 24]);
        assert_eq!(w.pending(), 0, "a full buffer is handed over");
        w.flush();
        let mut got = Vec::new();
        std::fs::File::open(&path).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, [[1u8; 40].as_slice(), &[2u8; 40], &[3u8; 24]].concat());
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
