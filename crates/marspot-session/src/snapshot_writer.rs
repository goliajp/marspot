//! Off-loop writer for the periodic crash-safety snapshot.
//!
//! Sibling of `control_writer` — same reason for existing, same shape.
//! Serialising a snapshot is cheap and in-memory (`state.bin` runs
//! ~100–300 KB), but the `write` + `rename` that follow are synchronous
//! disk IO, and disk IO on the L3 main loop is a stall waiting for a
//! busy disk.  Measured in the field: a 115 KB snapshot took **2.87 s**
//! while a build hammered the same disk, and the pane was frozen for all
//! of it.  Size was never the problem; contention was.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};

use marspot_term::lx_warn;

/// Queue handle for the snapshot writer thread.
///
/// **Latest-wins.**  The thread drains everything queued and writes only
/// the newest entry: an older snapshot of the same session has no value
/// once a newer one exists.  That is also what bounds memory without a
/// cap — the queue cannot outgrow the producer's 30 s cadence.
pub struct SnapshotWriter {
    tx: Sender<(PathBuf, Vec<u8>)>,
}

impl SnapshotWriter {
    pub fn spawn(session_id: u64) -> Self {
        let (tx, rx) = channel::<(PathBuf, Vec<u8>)>();
        std::thread::Builder::new()
            .name(format!("l3-snapshot-writer-{session_id}"))
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut latest = first;
                    while let Ok(next) = rx.try_recv() {
                        latest = next;
                    }
                    let (path, body) = latest;
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
        Self { tx }
    }

    /// Queue a snapshot body.  Never blocks.  `false` = the writer
    /// thread is gone.
    pub fn send(&self, path: PathBuf, body: Vec<u8>) -> bool {
        self.tx.send((path, body)).is_ok()
    }
}
