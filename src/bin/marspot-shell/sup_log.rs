//! Supervisor event log — writes to BOTH the legacy `supervisor.log`
//! file AND the new structured `marspot.log` stream via `logx`.
//!
//! Until every test script that greps the supervisor's event history
//! is migrated off the legacy path, this stays a dual-write shim:
//!
//! - **Legacy**: append-only TSV (`unix.f64 \t TAG \t detail \n`) at
//!   `paths::supervisor_log()` with a 2 MiB head-trim cap. Same shape
//!   it was before logx existed. Twenty soak / integration scripts
//!   grep this file by tag.
//!
//! - **New**: a `logx::event(Info, tag, detail, &[])` call so the same
//!   event ALSO lands in the structured `marspot.log` with pid / tid /
//!   ms / component fields. Future call sites add structured fields
//!   via `lx_event!` directly and skip this shim.
//!
//! The two writes are independent — a logx-side failure doesn't drop
//! the legacy event, and a legacy-side failure doesn't drop the logx
//! event. After PR 3, when every consumer has migrated to `marspot.log`,
//! the legacy write disappears and this module collapses to one line.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

const MAX_BYTES: u64 = 2 * 1024 * 1024;

static WRITER: Mutex<Option<File>> = Mutex::new(None);

fn log_dir() -> PathBuf {
    marspot::paths::log_dir()
}

fn log_path() -> PathBuf {
    marspot::paths::supervisor_log()
}

fn ensure_open() -> Option<std::sync::MutexGuard<'static, Option<File>>> {
    let mut guard = WRITER.lock().ok()?;
    if guard.is_none() {
        if std::fs::create_dir_all(log_dir()).is_err() {
            return None;
        }
        match OpenOptions::new().create(true).append(true).open(log_path()) {
            Ok(f) => *guard = Some(f),
            Err(_) => return None,
        }
    }
    Some(guard)
}

fn now_secs_f64() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Append one supervisor event. Goes to BOTH `supervisor.log` (legacy
/// TSV) and `marspot.log` (structured via logx). Cheap; never panics.
pub fn log(tag: &str, detail: &str) {
    // Structured stream first so logx-side failure doesn't lose the
    // event from the new pipeline; the legacy append is best-effort.
    marspot::logx::event(marspot::logx::Level::Info, tag, detail, &[]);

    let mut guard = match ensure_open() {
        Some(g) => g,
        None => return,
    };
    let file = match guard.as_mut() {
        Some(f) => f,
        None => return,
    };
    let safe_detail = detail.replace(['\n', '\t'], " ");
    let line = format!("{}\t{}\t{}\n", now_secs_f64(), tag, safe_detail);
    if file.write_all(line.as_bytes()).is_err() {
        return;
    }
    // Rotate the head off when the file grows past the cap.
    if let Ok(meta) = file.metadata() {
        if meta.len() > MAX_BYTES {
            let path = log_path();
            if let Ok(bytes) = std::fs::read(&path) {
                let keep_from = bytes.len() / 2;
                let snap = bytes[keep_from..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|i| keep_from + i + 1)
                    .unwrap_or(keep_from);
                let tail = &bytes[snap..];
                if let Ok(mut f) = OpenOptions::new().write(true).truncate(true).open(&path) {
                    let _ = f.write_all(tail);
                    *guard = Some(f);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn now_is_monotonic_within_a_call() {
        let a = super::now_secs_f64();
        let b = super::now_secs_f64();
        assert!(b >= a);
    }

    #[test]
    fn log_path_under_state_root() {
        let p = super::log_path();
        assert!(p.ends_with("supervisor.log"));
    }
}
