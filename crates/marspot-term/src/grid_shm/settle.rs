//! Reading a frame once, as a reader that is not poked for the next
//! publish must.
//!
//! A read spins a bounded number of times while the writer is between
//! the two stores of a publish.  On a loaded host the writer can be
//! taken off the cpu inside that window for longer than any spin, and
//! the read comes back empty although a frame is published.  The core
//! is poked for every publish and reads again; a caller that reads once
//! (a pane's screen read for a client, a test) would answer with no
//! screen at all.  So it gives the writer the cpu until the publish is
//! done, for at most [`WAIT_FOR_WRITER`]: a writer that died inside the
//! window never finishes it.

use std::time::{Duration, Instant};

pub(crate) const WAIT_FOR_WRITER: Duration = Duration::from_millis(100);

/// `read` until it finds a frame, while `seq` says a publish is under
/// way.  A read that fails twice at the same even `seq` fails for what
/// the frame is (no size, another layout), and is not waited out.
pub(crate) fn settled<T>(seq: impl Fn() -> u64, mut read: impl FnMut() -> Option<T>) -> Option<T> {
    let until = Instant::now() + WAIT_FOR_WRITER;
    let mut failed_at: Option<u64> = None;
    loop {
        if let Some(v) = read() {
            return Some(v);
        }
        let s = seq();
        if s == 0 || failed_at == Some(s) || Instant::now() >= until {
            return None;
        }
        if s & 1 == 0 {
            // the publish may have ended between the read and this look
            failed_at = Some(s);
            continue;
        }
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GridShmReader, GridShmWriter, Header, create_region};
    use super::*;
    use crate::grid::Grid;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;

    /// A writer stopped inside a publish longer than a read spins: the
    /// one-shot read keeps reading until the publish is done.  Driven by
    /// counts rather than a second thread, whose wake-up on a loaded host
    /// can come after the deadline.
    #[test]
    fn a_read_once_waits_out_a_writer_stopped_inside_a_publish() {
        use std::cell::Cell;
        let looks = Cell::new(0u32);
        let seq = || {
            looks.set(looks.get() + 1);
            if looks.get() < 50 { 5 } else { 6 }
        };
        let mut reads = 0;
        let got = settled(seq, || {
            reads += 1;
            (looks.get() >= 50).then_some(())
        });
        assert!(got.is_some(), "the frame once the publish is done");
        assert!(reads > 50, "it read again while the publish was under way: {reads}");
    }

    /// A writer that died inside a publish does not hold the read.
    #[test]
    fn a_read_once_gives_up_on_a_writer_that_never_finishes() {
        let fd = create_region(12, 3).unwrap();
        let r = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        w.publish(&Grid::new(12, 3), 0, 0);
        unsafe { (*(r.base_for_tests() as *const Header)).seq.fetch_add(1, Ordering::AcqRel) };
        let t0 = Instant::now();
        assert!(r.read_settled(&mut Vec::new(), &mut Vec::new()).is_none());
        let took = t0.elapsed();
        assert!(took >= WAIT_FOR_WRITER && took < WAIT_FOR_WRITER * 5, "{took:?}");
    }

    /// A frame that cannot be read for what it is fails at once.
    #[test]
    fn a_frame_unreadable_at_rest_is_not_waited_for() {
        let mut reads = 0;
        let t0 = Instant::now();
        assert!(settled(|| 4, || -> Option<()> {
            reads += 1;
            None
        })
        .is_none());
        assert_eq!(reads, 2, "read again once, in case a publish ended between");
        assert!(t0.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_region_never_published_is_not_waited_for() {
        let fd = create_region(12, 3).unwrap();
        let r = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
        let t0 = Instant::now();
        assert!(r.read_settled(&mut Vec::new(), &mut Vec::new()).is_none());
        assert!(t0.elapsed() < Duration::from_millis(50));
    }
}
