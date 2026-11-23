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
//! * **Idle buffers give their pages back.**  Once output has stopped,
//!   the buffers a burst wrote far into keep their allocation but hand
//!   their pages back to the system (see [`crate::idle_pages`]): the
//!   writer does it for the free ones when it has nothing left to do,
//!   the producer for the one it holds at the end of a burst.  The
//!   first 16 KiB of each stay, so the next small burst takes nothing
//!   back; the first write past them in a buffer takes the rest back.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;

mod files;
mod queue;
pub use files::{Job, behind, behind_with, retry_with};
use queue::{Buf, Cut, Msg, Queue};

/// What a writer thread writes to.
pub struct Files {
    pub main: Target,
    /// Where the side bytes go, for a writer that has been given one.
    pub side: Option<Target>,
}

/// One file a writer thread writes to, and where.
pub struct Target {
    pub file: Arc<std::fs::File>,
    /// The offset the next bytes go to, moved on by every write; `None`
    /// appends instead.
    pub at: Option<u64>,
    /// Where in the file the offset after each write is stated, as
    /// eight bytes little-endian, once the write is done: how a reader
    /// of a file that does not grow learns how much of it is written.
    pub publish: Option<u64>,
}

impl Target {
    /// Appending to `file`.
    pub fn appending(file: std::fs::File) -> Self {
        Self { file: Arc::new(file), at: None, publish: None }
    }

    /// Writing `file` from offset `at` on.
    pub fn at(file: Arc<std::fs::File>, at: u64) -> Self {
        Self { file, at: Some(at), publish: None }
    }
}

pub struct AsyncWriter {
    queue: Arc<Queue>,
    buf: Buf,
    /// How full `buf` may get before a write takes the slow path: `cap`,
    /// or less while some of its pages are given back.
    room: usize,
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
        Self::to(Files { main: Target::appending(file), side: None }, cap, depth)
    }

    /// [`Self::new`], to `files` as they are given.
    pub fn to(files: Files, cap: usize, depth: usize) -> Self {
        Self::woken_by(files, cap, depth, files::retry)
    }

    /// [`Self::to`], with what the writer thread calls once it has
    /// written what made it refuse a [`Self::hand_off`].
    fn woken_by(files: Files, cap: usize, depth: usize, retry: impl Fn() + Send + 'static) -> Self {
        // two free buffers, so the first few hand-overs don't allocate
        let queue = Arc::new(Queue::new(depth, 2, cap));
        let taken = Arc::clone(&queue);
        let handle = std::thread::Builder::new()
            .name("marspot-async-writer".into())
            .spawn(move || files::write_queued(files, depth, &taken, retry))
            .expect("spawn async writer");
        Self {
            queue,
            buf: Buf::sized(cap),
            room: cap,
            spare: None,
            cap,
            handle: Some(handle),
        }
    }

    /// Fill `buf` from here on; the one filled until now is returned.
    fn swap_in(&mut self, buf: Buf) -> Buf {
        self.room = buf.given.room(self.cap);
        std::mem::replace(&mut self.buf, buf)
    }

    /// The end of a burst: the empty buffer held, if a burst wrote far
    /// into it, gives its pages back, all but what the next small burst
    /// needs.
    fn rest(&mut self) {
        if self.buf.worth_giving_back() {
            self.buf.give_back();
            self.room = self.buf.given.room(self.cap);
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
        self.queue.push(Msg::Side(Target::appending(side)), true);
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
        let full = self.swap_in(empty);
        self.queue.push(Msg::Data(full), false);
    }

    /// An empty buffer to fill: the one a flood fills next, or one a
    /// flush or a write too big for the rest of a buffer starts.
    ///
    /// It keeps any pages it has given back until a write needs them.
    /// Taken back here, the pages of a buffer the output then ended on,
    /// or that a flush handed over empty, stayed taken: nothing had been
    /// written far into it, so nothing gave them back again.
    fn take_empty(&mut self) -> Buf {
        match self.spare.take().or_else(|| self.queue.take_free()) {
            Some(mut b) => {
                b.clear();
                b
            }
            None => Buf::sized(self.cap),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        // one comparison for the write that stays inside the buffer:
        // the scrollback makes one of these for every line
        if self.buf.main.len() + bytes.len() >= self.room {
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
    ///
    /// A buffer whose pages are given back comes here at the first
    /// write past what it kept, and takes them back; one that is handed
    /// over instead keeps them given.
    #[cold]
    fn write_to_the_brim(&mut self, bytes: &[u8]) {
        if !self.buf.main.is_empty() && self.buf.main.len() + bytes.len() > self.cap {
            self.hand_over();
        }
        if self.buf.main.len() + bytes.len() > self.room {
            self.buf.take_back();
            self.room = self.cap;
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
    /// is moments away.  When the output ends there instead, no buffer
    /// follows: the writer calls what [`retry_with`] was given once it
    /// has written what kept it busy, and the producer hands over
    /// again.  Without that the last lines of a flood stayed in memory
    /// until the pane next printed something.
    ///
    /// The buffer it swaps in keeps any pages it has given back: what
    /// the next burst writes into it is likely small.
    pub fn hand_off(&mut self) {
        if self.buf.is_empty() {
            self.queue.hurry();
            self.rest();
            return;
        }
        let mut empty = match self.spare.take().or_else(|| self.queue.take_free()) {
            Some(empty) => empty,
            None if self.queue.owe() => return,
            // the writer finished between the two looks
            None => self.take_empty(),
        };
        empty.clear();
        let pending = self.swap_in(empty);
        if let Err(Msg::Data(back)) = self.queue.try_push(Msg::Data(pending)) {
            self.spare = Some(self.swap_in(back));
            return;
        }
        self.rest();
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
        let pending = self.swap_in(empty);
        self.queue.push(Msg::Barrier(pending, ack_tx), true);
        let _ = ack_rx.recv();
    }

    /// Which thread writes.
    #[cfg(test)]
    pub(crate) fn thread_id(&self) -> std::thread::ThreadId {
        self.handle.as_ref().expect("a writer thread").thread().id()
    }

    /// Have the writer thread run `then` on its files once it has
    /// written everything written so far; what is written from here on
    /// goes to the files as `then` leaves them.
    ///
    /// Nothing is handed over and nothing is waited for: the cut
    /// travels in the buffer being filled, at the point it was made,
    /// so a producer that changes files pays for it what it pays for a
    /// write.
    pub fn cut(&mut self, then: impl FnOnce(&mut Files) + Send + 'static) {
        // one cut to a buffer
        if self.buf.cut.is_some() {
            self.hand_over();
        }
        self.buf.cut = Some(Cut { main_at: self.buf.main.len(), side_at: self.buf.side.len(), then: Box::new(then) });
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
mod idle_tests;

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

    /// Order across a change of files: what was written before a cut is
    /// in the files of before it, what was written after it in those it
    /// made, main and side alike -- in one buffer, in buffers handed
    /// over together, and with a second cut on the heels of the first.
    #[test]
    fn a_cut_parts_what_was_written_before_it_from_what_comes_after() {
        let open = |name: &str| {
            let path = tmp(name);
            (std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap(), path)
        };
        let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap();
        let (main, main_path) = open("cut-main-0");
        let (side, side_path) = open("cut-side-0");
        let mut paths = vec![(main_path, side_path)];
        // a buffer takes two or three of the writes below
        let mut w = AsyncWriter::new(main, 64, 3);
        w.pair(side);
        let mut want = vec![(String::new(), String::new())];
        for i in 0..40u32 {
            // two cuts in a row now and then: the second finds the
            // first still in the buffer
            for _ in 0..(usize::from(i % 3 == 0) + usize::from(i % 12 == 0)) {
                let n = paths.len();
                let (main, main_path) = open(&format!("cut-main-{n}"));
                let (side, side_path) = open(&format!("cut-side-{n}"));
                paths.push((main_path, side_path));
                want.push((String::new(), String::new()));
                w.cut(move |files| {
                    files.main = Target::appending(main);
                    files.side = Some(Target::appending(side));
                });
            }
            let (m, s) = (format!("m{i:03};"), format!("s{i};"));
            w.write(m.as_bytes());
            w.write_side(s.as_bytes());
            let now = want.last_mut().unwrap();
            now.0.push_str(&m);
            now.1.push_str(&s);
        }
        w.flush();
        for (k, ((main_path, side_path), (main, side))) in paths.iter().zip(&want).enumerate() {
            assert_eq!(&read(main_path), main, "main file {k}");
            assert_eq!(&read(side_path), side, "side file {k}");
            let _ = std::fs::remove_file(main_path);
            let _ = std::fs::remove_file(side_path);
        }
    }

    /// Writing at an offset puts the bytes there, one write after the
    /// other, leaves what is around them alone, and states where it got
    /// to where it was asked to.
    #[test]
    fn a_target_at_an_offset_is_written_in_place_and_says_how_far() {
        let path = tmp("at");
        std::fs::write(&path, vec![b'.'; 4096]).unwrap();
        let file = Arc::new(std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap());
        let mut main = Target::at(Arc::clone(&file), 100);
        main.publish = Some(0);
        let mut w = AsyncWriter::to(Files { main, side: None }, 16, 3);
        for i in 0..50u8 {
            w.write(&[b'a' + i % 26; 7]);
        }
        w.flush();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len(), 4096, "the file did not grow");
        assert_eq!(u64::from_le_bytes(got[0..8].try_into().unwrap()), 100 + 350);
        assert!(got[8..100].iter().all(|&b| b == b'.'));
        for i in 0..50usize {
            assert!(got[100 + i * 7..100 + i * 7 + 7].iter().all(|&b| b == b'a' + (i % 26) as u8), "write {i}");
        }
        assert!(got[450..].iter().all(|&b| b == b'.'));
        drop(w);
        let _ = std::fs::remove_file(&path);
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

    /// Output that ends on a refused hand-off is not left in memory:
    /// the writer says when it has room, and the hand-off made then
    /// goes through.  The file is a pipe nobody reads at first, so the
    /// writer is busy with every buffer for as long as the test wants.
    #[test]
    fn a_hand_off_refused_by_a_busy_writer_is_asked_for_again() {
        use std::os::unix::ffi::OsStrExt;
        const CAP: usize = 256 * 1024;
        let path = tmp("handoff-retry");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "a pipe to write into");
        let pipe = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let mut reader = std::fs::File::open(&path).unwrap();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        let mut w = AsyncWriter::woken_by(Files { main: Target::appending(pipe), side: None }, CAP, 4, move || {
            let _ = asked_tx.send(());
        });
        // more than the pipe holds, in both of the buffers there are
        w.write(&[1u8; CAP]);
        w.write(&[2u8; CAP]);
        w.write(&[3u8; 100]);
        w.hand_off();
        assert_eq!(w.pending(), 100, "the writer holds every other buffer");
        assert!(asked.try_recv().is_err(), "nothing was written yet");

        let mut flood = vec![0u8; 2 * CAP];
        reader.read_exact(&mut flood).unwrap();
        assert!(flood[..CAP].iter().all(|&b| b == 1) && flood[CAP..].iter().all(|&b| b == 2));
        asked
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the writer asks for the hand-off it refused");
        w.hand_off();
        assert_eq!(w.pending(), 0);
        let mut tail = [0u8; 100];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [3u8; 100]);
        assert!(asked.try_recv().is_err(), "asked once");
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
