//! The file a Gemini session's mail waits in.
//!
//! Nothing can put text into a running Gemini session from outside, so
//! what arrives at its inbox is kept here until the session's own hook
//! asks for it at a turn boundary (`gemini_hook`).  One file per
//! session, one JSON object per line: `{"text": …}`, the text being
//! what the session is to be shown.
//!
//! marspot appends and the hook takes, from different processes.  Both
//! hold an exclusive lock on the file while they work, and both check,
//! once they have it, that the path still names the file they opened:
//! a writer that opened the file just before it was taken would
//! otherwise write into a file nobody will read again, and a second
//! taker would read what the first already has.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::json::{self, Value, obj};

/// Messages one session's file holds before the next is refused.
pub const MAX_PENDING: usize = 64;

/// A session id that can be a file name: what Gemini itself accepts
/// for `--session-id`.
pub fn is_session_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The file of `session` in `dir`.
pub fn path_for(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Add `text` to the file at `path`, creating it.  False, and nothing
/// written, when `cap` messages already wait there.
pub fn append(path: &Path, text: &str, cap: usize) -> io::Result<bool> {
    loop {
        let mut f = OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(path)?;
        lock(&f)?;
        if !names(path, &f) {
            continue; // taken between the open and the lock
        }
        let mut held = String::new();
        f.read_to_string(&mut held)?;
        if held.lines().count() >= cap {
            return Ok(false);
        }
        let mut line = obj([("text", Value::from(text))]).to_json();
        line.push('\n');
        f.write_all(line.as_bytes())?;
        return Ok(true);
    }
}

/// Everything waiting at `path`, oldest first, and the file removed.
/// Of two takers one gets the messages and the other gets none.  Empty
/// on any failure: a file that could not be removed is not handed
/// over, since it would be handed over again at every later turn.
pub fn take(path: &Path) -> Vec<String> {
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    if lock(&f).is_err() || !names(path, &f) {
        return Vec::new();
    }
    let mut held = String::new();
    if f.read_to_string(&mut held).is_err() || fs::remove_file(path).is_err() {
        return Vec::new();
    }
    held.lines().filter_map(|l| json::parse(l).ok()?.str_at("text").map(str::to_string)).collect()
}

/// An exclusive lock for as long as `f` stays open.
fn lock(f: &File) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Whether `path` still names the file open as `f`.
fn names(path: &Path, f: &File) -> bool {
    match (fs::metadata(path), f.metadata()) {
        (Ok(at_path), Ok(open)) => at_path.ino() == open.ino() && at_path.dev() == open.dev(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("marspot-peer-spool-{}", crate::rand::hex(8)));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn messages_wait_in_order_and_are_taken_once() {
        let d = dir();
        let p = path_for(&d, "0a1b-session");
        assert!(take(&p).is_empty(), "no file, nothing waiting");
        assert!(append(&p, "first\nwith a second line and \"quotes\"", MAX_PENDING).unwrap());
        assert!(append(&p, "second", MAX_PENDING).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 2, "one object per line");
        assert_eq!(take(&p), vec!["first\nwith a second line and \"quotes\"".to_string(), "second".to_string()]);
        assert!(!p.exists());
        assert!(take(&p).is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_full_file_refuses_the_next_message_until_it_is_taken() {
        let d = dir();
        let p = path_for(&d, "s");
        for i in 0..3 {
            assert!(append(&p, &format!("m{i}"), 3).unwrap());
        }
        assert!(!append(&p, "m3", 3).unwrap());
        assert_eq!(take(&p), vec!["m0", "m1", "m2"]);
        assert!(append(&p, "m3", 3).unwrap(), "room again once taken");
        fs::remove_dir_all(&d).unwrap();
    }

    /// Two hooks of one session at the same turn boundary: the messages
    /// are shown once.
    #[test]
    fn of_two_takers_only_one_gets_the_messages() {
        let d = dir();
        let p = path_for(&d, "s");
        for round in 0..50 {
            assert!(append(&p, &format!("m{round}"), MAX_PENDING).unwrap());
            let takers: Vec<_> = (0..2)
                .map(|_| {
                    let p = p.clone();
                    std::thread::spawn(move || take(&p))
                })
                .collect();
            let got: Vec<Vec<String>> = takers.into_iter().map(|t| t.join().unwrap()).collect();
            let full = got.iter().filter(|g| !g.is_empty()).count();
            assert_eq!(full, 1, "round {round}: {got:?}");
        }
        fs::remove_dir_all(&d).unwrap();
    }

    /// A message written while the file is being taken lands in this
    /// take or the next; none is written into a file already taken.
    #[test]
    fn nothing_is_lost_between_a_writer_and_a_taker() {
        const TOTAL: usize = 400;
        let d = dir();
        let p = path_for(&d, "s");
        let taken = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let taker = {
            let (p, taken, done) = (p.clone(), taken.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    taken.fetch_add(take(&p).len(), Ordering::SeqCst);
                }
            })
        };
        for i in 0..TOTAL {
            assert!(append(&p, &format!("m{i}"), usize::MAX).unwrap());
        }
        done.store(true, Ordering::SeqCst);
        taker.join().unwrap();
        assert_eq!(taken.load(Ordering::SeqCst) + take(&p).len(), TOTAL);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_plain_ids_become_file_names() {
        assert!(is_session_name("adc1c168-0d82-4875-9e77-9647b1774641"));
        assert!(is_session_name("my_session-2"));
        for bad in ["", "../x", "a/b", "-rf", "a.b", "a b"] {
            assert!(!is_session_name(bad), "{bad:?}");
        }
    }
}
