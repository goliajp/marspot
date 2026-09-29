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
//! * **Bounded, so it cannot become a memory leak.**  The channel holds
//!   a fixed number of buffers; when the disk falls behind, the
//!   producer blocks on `send` and the backpressure propagates the way
//!   it always did (through the pty's own queue to the child).  A pane
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

use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

enum Msg {
    /// A full buffer to append.  Returned to the free list afterwards.
    Data(Vec<u8>),
    /// Append this, then acknowledge — the barrier `flush()` waits on.
    Barrier(Vec<u8>, SyncSender<()>),
    /// Cut the file back to `len` (a clear / restart).  Goes through
    /// the queue so it cannot race the writes ahead of it.
    Truncate(u64, SyncSender<()>),
    /// Replace the file being written (rotation).  Anything queued
    /// ahead of it still lands in the old file, which is what makes a
    /// rotation atomic from the producer's point of view.
    Swap(std::fs::File, SyncSender<()>),
}

pub struct AsyncWriter {
    tx: SyncSender<Msg>,
    free_rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    /// An empty buffer a refused `hand_off` got back, kept for the next
    /// swap rather than freed — see [`AsyncWriter::hand_off`].
    spare: Option<Vec<u8>>,
    cap: usize,
    handle: Option<JoinHandle<()>>,
}

impl AsyncWriter {
    /// `cap` is the handoff size — the producer blocks only once this
    /// much has accumulated.  `depth` buffers may be in flight; total
    /// memory is bounded by `cap * (depth + 2)`.
    pub fn new(file: std::fs::File, cap: usize, depth: usize) -> Self {
        let (tx, rx) = sync_channel::<Msg>(depth);
        let (free_tx, free_rx) = sync_channel::<Vec<u8>>(depth + 2);
        // Seed the free list so the first few handoffs don't allocate.
        for _ in 0..2 {
            let _ = free_tx.try_send(Vec::with_capacity(cap));
        }
        let handle = std::thread::Builder::new()
            .name("marspot-async-writer".into())
            .spawn(move || {
                let mut file = file;
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Data(mut b) => {
                            let _ = file.write_all(&b);
                            b.clear();
                            // Dropping the buffer when the free list is
                            // full is fine: it just means the next
                            // handoff allocates.  Never block here —
                            // the writer thread blocking on the free
                            // list would deadlock against a producer
                            // blocked on `send`.
                            let _ = free_tx.try_send(b);
                        }
                        Msg::Barrier(mut b, ack) => {
                            let _ = file.write_all(&b);
                            b.clear();
                            let _ = free_tx.try_send(b);
                            let _ = ack.send(());
                        }
                        Msg::Truncate(len, ack) => {
                            let _ = file.flush();
                            let _ = file.set_len(len);
                            let _ = ack.send(());
                        }
                        Msg::Swap(new_file, ack) => {
                            let _ = file.flush();
                            file = new_file;
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .expect("spawn async writer");
        Self {
            tx,
            free_rx,
            buf: Vec::with_capacity(cap),
            spare: None,
            cap,
            handle: Some(handle),
        }
    }

    fn take_empty(&mut self) -> Vec<u8> {
        if let Some(b) = self.spare.take() {
            return b;
        }
        match self.free_rx.try_recv() {
            Ok(mut b) => {
                b.clear();
                b
            }
            Err(_) => Vec::with_capacity(self.cap),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= self.cap {
            let empty = self.take_empty();
            let full = std::mem::replace(&mut self.buf, empty);
            let _ = self.tx.send(Msg::Data(full));
        }
    }

    /// Hand over what is buffered now, if the writer can take it without
    /// the producer waiting.  Called at the end of a burst.
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
            return;
        }
        let Some(mut empty) = self.spare.take().or_else(|| self.free_rx.try_recv().ok()) else {
            return;
        };
        empty.clear();
        let pending = std::mem::replace(&mut self.buf, empty);
        if let Err(e) = self.tx.try_send(Msg::Data(pending)) {
            let (TrySendError::Full(Msg::Data(back)) | TrySendError::Disconnected(Msg::Data(back))) = e
            else {
                return; // only `Data` is ever sent from here
            };
            self.spare = Some(std::mem::replace(&mut self.buf, back));
        }
    }

    /// Bytes handed over but not yet known to be on disk.  Callers that
    /// track file offsets themselves (the scrollback index) need this
    /// to reason about what a reader would see.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Wait until everything written so far has reached the file.
    pub fn flush(&mut self) {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        let empty = self.take_empty();
        let pending = std::mem::replace(&mut self.buf, empty);
        if self.tx.send(Msg::Barrier(pending, ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Discard everything queued and cut the file to `len`.
    pub fn truncate(&mut self, len: u64) {
        // Anything still buffered belongs to the content being
        // discarded — drop it rather than writing it past the new end.
        self.buf.clear();
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        if self.tx.send(Msg::Truncate(len, ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Flush, then continue appending to `new_file`.
    pub fn swap_file(&mut self, new_file: std::fs::File) {
        self.flush();
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        if self.tx.send(Msg::Swap(new_file, ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        self.flush();
        // Dropping the sender ends the writer loop; join so the file is
        // closed before we return (tests reopen these paths immediately).
        let tx = std::mem::replace(&mut self.tx, sync_channel::<Msg>(1).0);
        drop(tx);
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
