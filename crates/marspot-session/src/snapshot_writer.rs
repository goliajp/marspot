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

use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};

use marspot_term::lx_warn;

enum Job {
    Snapshot(PathBuf, Vec<u8>),
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
    pub fn spawn(session_id: u64) -> Self {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name(format!("l3-snapshot-writer-{session_id}"))
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut latest = None;
                    for job in std::iter::once(first).chain(rx.try_iter()) {
                        match job {
                            Job::Snapshot(path, body) => latest = Some((path, body)),
                            Job::Delete(path) => {
                                let _ = std::fs::remove_file(path);
                            }
                        }
                    }
                    let Some((path, body)) = latest else { continue };
                    let tmp = path.with_extension("bin.tmp");
                    let r = std::fs::write(&tmp, &body)
                        .and_then(|()| std::fs::rename(&tmp, &path));
                    if let Err(e) = r {
                        lx_warn!(
                            "session.periodic_snapshot_failed",
                            &format!("{e}"),
                            path = path.display()
                        );
                    }
                }
            })
            .expect("spawn l3-snapshot-writer thread");
        marspot_term::async_writer::delete_with(Self::deleter(tx.clone()));
        Self { tx }
    }

    /// Queue a snapshot body.  Never blocks.  `false` = the writer
    /// thread is gone.
    pub fn send(&self, path: PathBuf, body: Vec<u8>) -> bool {
        self.tx.send(Job::Snapshot(path, body)).is_ok()
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
        let writer = SnapshotWriter::spawn(0);
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
}
