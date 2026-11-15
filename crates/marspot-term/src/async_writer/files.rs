//! What the writer thread does to its files.

use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;
use std::sync::mpsc::SyncSender;

use super::queue::{Buf, Msg, Queue};
use super::{Files, Target};

/// The writer thread: what `queue` hands out goes to `files`, until the
/// producer is gone.
pub(super) fn write_queued(mut files: Files, depth: usize, queue: &Queue, retry: impl Fn()) {
    // buffers taken together and not yet written, and who waits to
    // hear that they have been
    let mut run: Vec<Buf> = Vec::with_capacity(depth);
    let mut acks: Vec<SyncSender<()>> = Vec::new();
    while let Some(batch) = queue.take() {
        for msg in batch {
            match msg {
                Msg::Data(b) => run.push(b),
                Msg::Barrier(b, ack) => {
                    run.push(b);
                    acks.push(ack);
                }
                Msg::Side(target) => {
                    append(&mut files, &mut run, &mut acks, queue);
                    files.side = Some(target);
                    queue.wrote();
                }
            }
        }
        append(&mut files, &mut run, &mut acks, queue);
        if queue.settle() {
            retry();
        }
    }
}

/// Write the buffers of `run` in one write to each file, hand them back
/// and answer `acks`.
///
/// One write for all of them: a write is a system call whatever it
/// carries, and a flood that was written a buffer at a time made
/// thousands of them a second.  The main bytes go first: the side file
/// is an index into them, and must never hold what they do not.  A
/// buffer with a cut in it is written up to there, the cut is made, and
/// the rest goes to the files as the cut left them.
fn append(files: &mut Files, run: &mut Vec<Buf>, acks: &mut Vec<SyncSender<()>>, queue: &Queue) {
    // the first buffer not yet written whole, and how much of it is
    let (mut at, mut from) = (0, (0, 0));
    while let Some(i) = (at..run.len()).find(|&i| run[i].cut.is_some()) {
        let cut = run[i].cut.take().expect("found just above");
        let upto = (cut.main_at, cut.side_at);
        write(files, &run[at..=i], from, Some(upto));
        (cut.then)(files);
        (at, from) = (i, upto);
    }
    if at < run.len() {
        write(files, &run[at..], from, None);
    }
    for mut b in run.drain(..) {
        b.clear();
        queue.wrote_back(b);
    }
    for ack in acks.drain(..) {
        let _ = ack.send(());
    }
}

/// `bufs` to the files in one write each, leaving out what comes before
/// `from` in the first of them and, with `upto`, what comes after it in
/// the last: (main bytes, side bytes) both.
fn write(files: &mut Files, bufs: &[Buf], from: (usize, usize), upto: Option<(usize, usize)>) {
    let last = bufs.len().wrapping_sub(1);
    let lo = |k: usize, from: usize| if k == 0 { from } else { 0 };
    let hi = |k: usize, len: usize, upto: Option<usize>| upto.filter(|_| k == last).unwrap_or(len);
    let main = bufs.iter().enumerate().map(|(k, b)| &b.main[lo(k, from.0)..hi(k, b.main.len(), upto.map(|u| u.0))]);
    write_to(&mut files.main, main);
    if let Some(side) = files.side.as_mut() {
        let side_bytes = bufs.iter().enumerate().map(|(k, b)| &b.side[lo(k, from.1)..hi(k, b.side.len(), upto.map(|u| u.1))]);
        write_to(side, side_bytes);
    }
}

/// Most buffers one write carries; more are written in turns.
const AT_ONCE: usize = 8;

/// Every one of `parts` to `target`, in order, in as few writes as the
/// kernel takes them in.
///
/// A target written at an offset moves on by what it was given whether
/// or not the system took all of it: the producer counted on that much
/// when it wrote offsets into the bytes that follow.
fn write_to<'a>(target: &mut Target, parts: impl Iterator<Item = &'a [u8]>) {
    let mut held = [std::io::IoSlice::new(&[]); AT_ONCE];
    let mut n = 0;
    let mut wrote = 0u64;
    let mut flush = |held: &[std::io::IoSlice<'a>]| {
        let total: usize = held.iter().map(|p| p.len()).sum();
        if total == 0 {
            return;
        }
        wrote += total as u64;
        match target.at.as_mut() {
            Some(at) => {
                write_all_at(&target.file, held, *at, total);
                *at += total as u64;
            }
            None => append_all(&target.file, held, total),
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
    if let (Some(publish), Some(at), true) = (target.publish, target.at, wrote > 0) {
        let _ = target.file.write_all_at(&at.to_le_bytes(), publish);
    }
}

fn append_all(file: &std::fs::File, held: &[std::io::IoSlice<'_>], total: usize) {
    let mut file = file;
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
}

fn write_all_at(file: &std::fs::File, held: &[std::io::IoSlice<'_>], at: u64, total: usize) {
    use std::os::fd::AsRawFd;
    // SAFETY: `IoSlice` is guaranteed to be ABI-compatible with `iovec`,
    // and every slice outlives the call.
    let n = unsafe { libc::pwritev(file.as_raw_fd(), held.as_ptr().cast(), held.len() as libc::c_int, at as libc::off_t) };
    let mut written = usize::try_from(n).unwrap_or(0);
    if written == total {
        return;
    }
    // a short or failed vectored write leaves the rest to plain ones
    let mut pos = at;
    for part in held {
        let skip = written.min(part.len());
        written -= skip;
        pos += skip as u64;
        let _ = file.write_all_at(&part[skip..], pos);
        pos += (part.len() - skip) as u64;
    }
}

/// Who is told that a writer has room for a hand-over it refused.
static RETRY: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Have `wake` called, on a writer's thread, each time a writer that
/// refused a hand-over has written what kept it busy.  A process says
/// this once, with what makes its producers hand over again; in one
/// that never does, refused bytes wait for the next full buffer or
/// flush.
pub fn retry_with(wake: impl Fn() + Send + Sync + 'static) {
    let _ = RETRY.set(Box::new(wake));
}

pub(super) fn retry() {
    if let Some(wake) = RETRY.get() {
        wake();
    }
}

/// Work on files that neither a parser nor a disk writer should wait
/// for: making a file and giving it its blocks, or deleting one, takes
/// the system up to tens of milliseconds now and then.
pub type Job = Box<dyn FnOnce() + Send>;

/// Who runs what [`behind`] is given.
static RUNNER: OnceLock<Box<dyn Fn(Job) + Send + Sync>> = OnceLock::new();

/// Have `runner` given every job [`behind`] is handed, to run in order
/// on a thread of the process's own.  A process says this once, if it
/// has such a thread.
pub fn behind_with(runner: impl Fn(Job) + Send + Sync + 'static) {
    let _ = RUNNER.set(Box::new(runner));
}

/// Run `job` where nobody waits for it: on the thread the process named
/// with [`behind_with`], or else on a thread of its own.
pub fn behind(job: Job) {
    match RUNNER.get() {
        Some(runner) => runner(job),
        None => {
            // shared with the thread, so that a thread that cannot be
            // made leaves the job to run here
            let job = std::sync::Arc::new(std::sync::Mutex::new(Some(job)));
            let theirs = std::sync::Arc::clone(&job);
            let take = |j: &std::sync::Mutex<Option<Job>>| j.lock().unwrap_or_else(|p| p.into_inner()).take();
            let spawned = std::thread::Builder::new().name("marspot-behind".into()).spawn(move || {
                if let Some(job) = take(&theirs) {
                    job();
                }
            });
            if spawned.is_err()
                && let Some(job) = take(&job)
            {
                job();
            }
        }
    }
}
