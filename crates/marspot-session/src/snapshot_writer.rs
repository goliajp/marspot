//! Off-loop writer for the periodic crash-safety snapshot.
//!
//! Sibling of `control_writer` — same reason for existing, same shape.
//! Serialising a snapshot is cheap and in-memory (`state.bin` runs
//! ~100–300 KB), but the `write` + `rename` that follow are synchronous
//! disk IO, and disk IO on the L3 main loop is a stall waiting for a
//! busy disk.  Measured in the field: a 115 KB snapshot took **2.87 s**
//! while a build hammered the same disk, and the pane was frozen for all
//! of it.  Size was never the problem; contention was.
//!
//! The same thread does the history files' work that nobody should
//! wait for -- making the next segment and giving it its blocks,
//! deleting one past the budget -- which takes the system up to tens of
//! milliseconds now and then, and neither the parser nor a disk writer
//! has that.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};

use marspot_term::lx_warn;

enum Job {
    Snapshot(PathBuf, Vec<u8>),
    /// A snapshot somebody waits for.
    Awaited(PathBuf, Vec<u8>, SyncSender<()>),
    Run(marspot_term::async_writer::Job),
}

/// Queue handle for the snapshot writer thread.
///
/// **Latest-wins.**  The thread drains everything queued and writes only
/// the newest snapshot: an older one of the same session has no value
/// once a newer one exists.  That is also what bounds memory without a
/// cap — the queue cannot outgrow the producer's 30 s cadence.  Every
/// job queued is run, in order.
pub struct SnapshotWriter {
    tx: Sender<Job>,
}

impl SnapshotWriter {
    /// Also tells the disk writers what this process does for them:
    /// their file work on this thread, and a `wake` of the main loop when one
    /// of them was too busy to take the end of a burst and has room
    /// again -- the turn that follows hands it over.
    pub fn spawn(session_id: u64, wake: Sender<crate::SessionEvent>) -> Self {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name(format!("l3-snapshot-writer-{session_id}"))
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut latest = None;
                    let mut awaited = None;
                    for job in std::iter::once(first).chain(rx.try_iter()) {
                        match job {
                            Job::Snapshot(path, body) => latest = Some((path, body)),
                            Job::Awaited(path, body, written) => {
                                latest = Some((path, body));
                                awaited = Some(written);
                            }
                            Job::Run(job) => job(),
                        }
                    }
                    let Some((path, body)) = latest else { continue };
                    if let Err(e) = Self::write(&path, &body) {
                        lx_warn!(
                            "session.periodic_snapshot_failed",
                            &format!("{e}"),
                            path = path.display()
                        );
                    }
                    if let Some(written) = awaited {
                        let _ = written.send(());
                    }
                }
            })
            .expect("spawn l3-snapshot-writer thread");
        marspot_term::async_writer::behind_with(Self::runner(tx.clone()));
        let wake = std::sync::Mutex::new(wake);
        marspot_term::async_writer::retry_with(move || {
            let _ = wake.lock().unwrap_or_else(|p| p.into_inner()).send(crate::SessionEvent::Wake);
        });
        Self { tx }
    }

    /// Queue a snapshot body.  Never blocks.  `false` = the writer
    /// thread is gone.
    pub fn send(&self, path: PathBuf, body: Vec<u8>) -> bool {
        self.tx.send(Job::Snapshot(path, body)).is_ok()
    }

    fn write(path: &Path, body: &[u8]) -> std::io::Result<()> {
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path))
    }

    /// Write the snapshot an image leaves for the one that replaces it,
    /// and return once it is in place.
    ///
    /// Through the thread, so that it is the last thing written there:
    /// a periodic snapshot still on its way to the disk when the image
    /// wrote this one itself was renamed over it afterwards, and the
    /// next image started from a screen up to one disk write old, with
    /// the lines printed since in neither the screen nor the history.
    pub fn write_last(&self, path: PathBuf, body: Vec<u8>) {
        let (written, wait) = sync_channel(1);
        match self.tx.send(Job::Awaited(path, body, written)) {
            Ok(()) => {
                let _ = wait.recv();
            }
            Err(e) => {
                let Job::Awaited(path, body, _) = e.0 else { return };
                let _ = Self::write(&path, &body);
            }
        }
    }

    /// Return once everything queued before has been done.
    pub fn settle(&self) {
        let (done, wait) = sync_channel(1);
        let job = Job::Run(Box::new(move || {
            let _ = done.send(());
        }));
        if self.tx.send(job).is_ok() {
            let _ = wait.recv();
        }
    }

    /// What runs a job on the thread `tx` leads to.  With that thread
    /// gone the job runs where the caller stands.
    fn runner(tx: Sender<Job>) -> impl Fn(marspot_term::async_writer::Job) + Send + Sync + 'static {
        let tx = std::sync::Mutex::new(tx);
        move |job| {
            let tx = tx.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = tx.send(Job::Run(job)) {
                let Job::Run(job) = e.0 else { return };
                job();
            }
        }
    }

    /// A job that deletes `path`, for the tests.
    #[cfg(test)]
    fn deleting(path: PathBuf) -> marspot_term::async_writer::Job {
        Box::new(move || {
            let _ = std::fs::remove_file(path);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job handed over is run soon after, whatever else is queued,
    /// and the snapshot queued with it is written.
    #[test]
    fn a_file_handed_over_is_deleted_and_the_snapshot_still_written() {
        let dir = std::env::temp_dir().join(format!("marspot-snapshot-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (doomed, other, state) = (dir.join("a.old"), dir.join("b.old"), dir.join("state.bin"));
        std::fs::write(&doomed, b"x").unwrap();
        std::fs::write(&other, b"y").unwrap();
        let writer = SnapshotWriter::spawn(0, channel().0);
        let run = SnapshotWriter::runner(writer.tx.clone());
        run(SnapshotWriter::deleting(doomed.clone()));
        assert!(writer.send(state.clone(), b"body".to_vec()));
        run(SnapshotWriter::deleting(other.clone()));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while doomed.exists() || other.exists() || std::fs::read(&state).ok().as_deref() != Some(b"body") {
            assert!(std::time::Instant::now() < deadline, "the queue was not worked off");
            std::thread::yield_now();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The snapshot written for the next image is the one it finds,
    /// though a periodic one was on its way to the disk when it was
    /// asked for.  The periodic one's temporary file is a pipe, so the
    /// thread is inside that write until the test reads it.
    #[test]
    fn the_snapshot_for_the_next_image_is_written_after_one_under_way() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("marspot-snapshot-last-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (state, tmp, sentinel) = (dir.join("state.bin"), dir.join("state.bin.tmp"), dir.join("c.old"));
        let c_tmp = std::ffi::CString::new(tmp.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_tmp.as_ptr(), 0o600) }, 0, "a pipe to write into");
        let writer = SnapshotWriter::spawn(0, channel().0);
        assert!(writer.send(state.clone(), b"periodic".to_vec()));
        // opens once the thread has opened it to write
        let mut pipe = std::fs::File::open(&tmp).unwrap();

        writer.write_last(state.clone(), b"for the next image".to_vec());
        let mut periodic = Vec::new();
        std::io::Read::read_to_end(&mut pipe, &mut periodic).unwrap();
        assert_eq!(periodic, b"periodic", "the thread was writing it");
        // everything queued has been worked off once this is gone
        std::fs::write(&sentinel, b"z").unwrap();
        SnapshotWriter::runner(writer.tx.clone())(SnapshotWriter::deleting(sentinel.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while sentinel.exists() {
            assert!(std::time::Instant::now() < deadline, "the queue was not worked off");
            std::thread::yield_now();
        }
        assert!(std::fs::metadata(&state).unwrap().is_file(), "the periodic write was renamed over it");
        assert_eq!(std::fs::read(&state).unwrap(), b"for the next image");
        let _ = std::fs::remove_dir_all(dir);
    }
}
