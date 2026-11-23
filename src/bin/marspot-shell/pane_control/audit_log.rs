//! The audit log's own thread and file: `<state dir>/control-audit.log`,
//! mode 0600, append only, rotated at 8 MiB keeping 8 files.
//!
//! The thread sleeps on its queue, and only while a merged line is held
//! does it also wake at the moment that line is due.  The queue is
//! bounded; what does not fit is counted and the count written with the
//! next line.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::audit::{Merger, Rec};

pub(super) const ROTATE_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const KEEP_FILES: usize = 8;
const QUEUE: usize = 1024;

enum Event {
    Rec(Rec),
    Refused { caller: String, code: &'static str, op: &'static str, pane: String, at_ms: u64 },
    /// Write out everything held, then answer.
    Flush(SyncSender<()>),
}

pub(crate) struct Audit {
    tx: SyncSender<Event>,
    dropped: Arc<AtomicU64>,
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub(super) fn log_path() -> PathBuf {
    marspot_term::paths::state_root().join("control-audit.log")
}

impl Audit {
    pub fn start() -> Audit {
        Audit::at(log_path(), ROTATE_BYTES)
    }

    pub fn at(path: PathBuf, rotate_at: u64) -> Audit {
        let (tx, rx) = sync_channel(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let d = Arc::clone(&dropped);
        let spawned = std::thread::Builder::new().name("control-audit".into()).spawn(move || run(rx, &path, rotate_at, &d));
        if let Err(e) = spawned {
            marspot::lx_warn!("shell.control.audit_thread_failed", &format!("{e}"));
        }
        Audit { tx, dropped }
    }

    fn send(&self, e: Event) {
        if self.tx.try_send(e).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record(&self, rec: Rec) {
        self.send(Event::Rec(rec));
    }

    pub fn refused(&self, caller: String, code: &'static str, op: &'static str, pane: String) {
        self.send(Event::Refused { caller, code, op, pane, at_ms: now_ms() });
    }

    /// Wait (briefly) until everything sent so far is in the file.
    pub fn flush(&self) {
        let (tx, rx) = sync_channel(1);
        if self.tx.send(Event::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

fn run(rx: Receiver<Event>, path: &Path, rotate_at: u64, dropped: &AtomicU64) {
    let mut m = Merger::default();
    let utc = |ms: u64| utc_text(ms);
    loop {
        let ev = match m.due() {
            Some(due) => match rx.recv_timeout(Duration::from_millis(due.saturating_sub(now_ms()))) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => return,
            },
        };
        let mut lines = match ev {
            None => m.flush_due(now_ms(), &utc),
            Some(Event::Rec(r)) => m.add(r, &utc),
            Some(Event::Refused { caller, code, op, pane, at_ms }) => m.refused(&caller, code, op, &pane, at_ms, &utc),
            Some(Event::Flush(ack)) => {
                let lines = m.flush_all(&utc);
                write_lines(path, rotate_at, &lines, dropped);
                let _ = ack.send(());
                continue;
            }
        };
        if !lines.is_empty() {
            write_lines(path, rotate_at, &std::mem::take(&mut lines), dropped);
        }
    }
}

fn write_lines(path: &Path, rotate_at: u64, lines: &[String], dropped: &AtomicU64) {
    if lines.is_empty() {
        return;
    }
    let lost = dropped.swap(0, Ordering::Relaxed);
    let mut body = String::new();
    if lost > 0 {
        body.push_str(&format!("{{\"at\":\"{}\",\"dropped_lines\":{lost}}}\n", utc_text(now_ms())));
    }
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() + body.len() as u64 > rotate_at) {
        rotate(path);
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path);
    match file {
        Ok(mut f) => {
            let _ = f.write_all(body.as_bytes());
        }
        Err(e) => marspot::lx_warn!("shell.control.audit_write_failed", &format!("{e}")),
    }
}

/// `log` → `log.1` → … → `log.7`; the oldest goes.
pub(super) fn rotate(path: &Path) {
    let numbered = |n: usize| PathBuf::from(format!("{}.{n}", path.display()));
    let _ = std::fs::remove_file(numbered(KEEP_FILES - 1));
    for n in (1..KEEP_FILES - 1).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

/// `ms` after the epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub(super) fn utc_text(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem / 60 % 60, rem % 60, ms % 1000)
}

/// The last `n` lines of the log, reaching into the file before it
/// when the log has fewer.
pub(super) fn tail(path: &Path, n: usize) -> Vec<String> {
    let mut out = last_lines(path, n);
    if out.len() < n {
        let mut older = last_lines(&PathBuf::from(format!("{}.1", path.display())), n - out.len());
        older.append(&mut out);
        out = older;
    }
    out
}

/// The last `n` lines of one file, read backwards from its end a block
/// at a time until they are all in.
pub(super) fn last_lines(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: u64 = 16 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let Ok(len) = f.seek(SeekFrom::End(0)) else { return Vec::new() };
    let (mut buf, mut at, mut newlines) = (Vec::new(), len, 0usize);
    // n whole lines need n + 1 newlines unless the file's start is in
    while at > 0 && newlines <= n {
        let step = BLOCK.min(at);
        at -= step;
        let mut block = vec![0; step as usize];
        if f.seek(SeekFrom::Start(at)).is_err() || f.read_exact(&mut block).is_err() {
            return Vec::new();
        }
        newlines += block.iter().filter(|b| **b == b'\n').count();
        block.append(&mut buf);
        buf = block;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if at > 0 && !lines.is_empty() {
        // cut mid-line
        lines.remove(0);
    }
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}
