//! Per-session append-only byte log.
//!
//! Records every byte read from a session's PTY master so a freshly
//! attached client gets the full history replayed (RFC-002 ATTACH path).
//! Capped at `BYTELOG_CAP_BYTES`; on overflow the most recent
//! `BYTELOG_RETAIN_BYTES` are copied to a fresh file and the old one
//! atomically replaced.
//!
//! Disk location: `~/Library/Caches/marspot/sessions/<id>/bytelog`.
//! Survives daemon restarts so reattach from a fresh process also gets
//! prior history (or, post-RFC-003, the freshly spawned L3 with the
//! same session id can rehydrate from its own past lifetime).
//!
//! Originally lived inline in `marspot-shelld`; lifted to
//! `marspot-term` so L3 (`marspot-session`) can own a bytelog directly
//! without going through L4 (RFC-003).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::paths::sessions_dir;

/// Per-session byte log cap. When the log file grows past this we
/// compact: keep the most recent `BYTELOG_RETAIN_BYTES` of bytes,
/// drop the rest. 100 MiB is generous — even a `cat /dev/urandom`
/// run for several seconds doesn't fill it.
pub const BYTELOG_CAP_BYTES: u64 = 100 * 1024 * 1024;
pub const BYTELOG_RETAIN_BYTES: u64 = 50 * 1024 * 1024;

pub struct ByteLog {
    path: PathBuf,
    file: File,
    bytes_written: u64,
}

impl ByteLog {
    pub fn open(session_id: u64) -> io::Result<Self> {
        let dir = sessions_dir().join(session_id.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("bytelog");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self { path, file, bytes_written })
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.bytes_written += bytes.len() as u64;
        if self.bytes_written > BYTELOG_CAP_BYTES {
            // Best-effort compaction: failure here just leaves the log
            // oversized until the next append tries again.
            let _ = self.compact();
        }
        Ok(())
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn compact(&mut self) -> io::Result<()> {
        let tmp_path = self.path.with_extension("tmp");
        {
            let mut src = OpenOptions::new().read(true).open(&self.path)?;
            let len = src.metadata()?.len();
            let keep_from = len.saturating_sub(BYTELOG_RETAIN_BYTES);
            src.seek(SeekFrom::Start(keep_from))?;
            let mut dst = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp_path)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n])?;
            }
        }
        std::fs::rename(&tmp_path, &self.path)?;
        // Reopen the file handle so append picks up the truncated state
        // (the previous fd points at the old inode).
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        self.bytes_written = self.file.metadata()?.len();
        Ok(())
    }
}

/// Remove a session's bytelog file and its session directory. Called
/// from the KILL_SESSION arm in shelld so a killed session doesn't
/// leave gigabytes of log behind. Best-effort — a failed remove just
/// leaves stale files until the next housekeeping sweep.
pub fn delete_bytelog(session_id: u64) {
    let dir = sessions_dir().join(session_id.to_string());
    let _ = std::fs::remove_dir_all(&dir);
}
