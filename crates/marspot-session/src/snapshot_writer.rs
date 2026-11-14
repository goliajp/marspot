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
//! The same thread deletes the bytelog segments a rotation retires:
//! giving one back takes the system 6 to 46 ms, which neither the
//! parser nor a disk writer has.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};

use marspot_term::lx_warn;

enum Job {
    Snapshot(PathBuf, Vec<u8>),
    /// A snapshot somebody waits for.
    Awaited(PathBuf, Vec<u8>, SyncSender<()>),
    Delete(PathBuf),
}

/// Queue handle for the snapshot writer thread.
///
/// **Latest-wins.**  The thread drains everything queued and writes only
/// the newest snapshot: an older one of the same session has no value
/// once a newer one exists.  That is also what bounds memory without a
/// cap — the queue cannot outgrow the producer's 30 s cadence.  Every
/// deletion queued is carried out.
pub struct SnapshotWriter {
    tx: Sender<Job>,
}

impl SnapshotWriter {
    /// Also tells the disk writers what this process does for them:
    /// deletes on this thread, and a `wake` of the main loop when one
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
                            Job::Delete(path) => {
                                let _ = std::fs::remove_file(path);
                            }
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
        marspot_term::async_writer::delete_with(Self::deleter(tx.clone()));
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

    /// What deletes a file on the thread `tx` leads to.  With that
    /// thread gone the file is deleted where the caller stands.
    fn deleter(tx: Sender<Job>) -> impl Fn(PathBuf) + Send + Sync + 'static {
        let tx = std::sync::Mutex::new(tx);
        move |path| {
            let tx = tx.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = tx.send(Job::Delete(path)) {
                let Job::Delete(path) = e.0 else { return };
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file handed to the deleter is gone soon after, whatever else
    /// is queued, and the snapshot queued with it is written.
    #[test]
    fn a_file_handed_over_is_deleted_and_the_snapshot_still_written() {
        let dir = std::env::temp_dir().join(format!("marspot-snapshot-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (doomed, other, state) = (dir.join("a.old"), dir.join("b.old"), dir.join("state.bin"));
        std::fs::write(&doomed, b"x").unwrap();
        std::fs::write(&other, b"y").unwrap();
        let writer = SnapshotWriter::spawn(0, channel().0);
        let delete = SnapshotWriter::deleter(writer.tx.clone());
        delete(doomed.clone());
        assert!(writer.send(state.clone(), b"body".to_vec()));
        delete(other.clone());

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
        SnapshotWriter::deleter(writer.tx.clone())(sentinel.clone());
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
