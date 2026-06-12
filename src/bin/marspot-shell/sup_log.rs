//! Append-only supervisor event log at
//! `~/Library/Logs/Marspot/supervisor.log`.
//!
//! Lifecycle events the shell tracks land here: every spawn,
//! crash, hello-ack, update-promote, rollback, and banner
//! transition.  Format is TSV — timestamp + tag + free-form
//! detail — so a `grep CRASH | tail` reads as a quick history of
//! what's been happening with the supervisor.
//!
//! Bounded by a soft size cap: when the file crosses
//! `MAX_BYTES`, we copy the trailing half into a new file and
//! truncate the original.  Keeps logs from growing without
//! bound on a long-running shell.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

/// Cap before we trim the head off.  ~2 MiB = several hundred
/// thousand events at the verbosity below.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Single shared writer per process.  Open is lazy — first
/// `log()` call creates the file.
static WRITER: Mutex<Option<File>> = Mutex::new(None);

fn log_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Logs/Marspot")
}

fn log_path() -> PathBuf {
    log_dir().join("supervisor.log")
}

fn ensure_open() -> Option<std::sync::MutexGuard<'static, Option<File>>> {
    let mut guard = WRITER.lock().ok()?;
    if guard.is_none() {
        if let Err(e) = std::fs::create_dir_all(log_dir()) {
            eprintln!("[shell] sup_log: mkdir {}: {e}", log_dir().display());
            return None;
        }
        let path = log_path();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => *guard = Some(f),
            Err(e) => {
                eprintln!("[shell] sup_log: open {}: {e}", path.display());
                return None;
            }
        }
    }
    Some(guard)
}

/// Wall-clock seconds since the Unix epoch as a float.  Used as
/// the timestamp in each log line.  Float seconds keep the
/// formatter trivial; humans cross-reference against `date`.
fn now_secs_f64() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Append one event.  `tag` is a short uppercase identifier
/// (`CRASH`, `HELLO_ACK`, …); `detail` is whatever context
/// the call site has.
pub fn log(tag: &str, detail: &str) {
    let mut guard = match ensure_open() {
        Some(g) => g,
        None => return,
    };
    let file = match guard.as_mut() {
        Some(f) => f,
        None => return,
    };
    // Strip newlines/tabs from detail so a single event is always
    // exactly one line; eases tail-and-tail-and-greps.
    let safe_detail = detail.replace(['\n', '\t'], " ");
    let line = format!("{}\t{}\t{}\n", now_secs_f64(), tag, safe_detail);
    if let Err(e) = file.write_all(line.as_bytes()) {
        eprintln!("[shell] sup_log: write: {e}");
    }
    // Size-cap check happens after the write so we never lose the
    // event itself — the rotation only ever sheds older history.
    if let Ok(meta) = file.metadata() {
        if meta.len() > MAX_BYTES {
            // Best-effort: read trailing half, truncate, rewrite.
            let path = log_path();
            if let Ok(bytes) = std::fs::read(&path) {
                let keep_from = bytes.len() / 2;
                // Snap to the next newline so we don't slice mid-line.
                let snap = bytes[keep_from..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|i| keep_from + i + 1)
                    .unwrap_or(keep_from);
                let tail = &bytes[snap..];
                if let Ok(mut f) = OpenOptions::new().write(true).truncate(true).open(&path) {
                    let _ = f.write_all(tail);
                    // Replace the open handle with the truncated file.
                    *guard = Some(f);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_secs_is_monotonic_within_a_call() {
        let a = now_secs_f64();
        let b = now_secs_f64();
        assert!(b >= a);
    }

    #[test]
    fn log_path_under_home() {
        // HOME is set in normal test contexts; if not, /tmp is used.
        let p = log_path();
        assert!(p.ends_with("Library/Logs/Marspot/supervisor.log"));
    }
}
