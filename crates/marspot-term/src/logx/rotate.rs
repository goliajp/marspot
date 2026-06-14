//! Cross-process log rotation: flock-coordinated rename + create + a
//! background compress + retention prune.
//!
//! Triggered from `sink::write_line` when the per-process estimate of
//! the active file's size crosses `max_bytes`, or when the time since
//! the last rotate crosses `ROTATE_AGE`. The rotating process holds
//! `flock(LOCK_EX | LOCK_NB)` on a sibling lock file while it does
//! the rename + new-active-open; siblings that lose the race detect
//! the rotated inode on their next size-check and reopen onto the
//! fresh file.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use super::sink::{Sink, ROTATE_AGE};

const DEFAULT_KEEP: usize = 10;

/// Called from `sink::write_line` after every `STAT_INTERVAL` writes
/// or when the cached byte estimate reaches `max_bytes`. Idempotent
/// no-op when neither condition holds.
pub(crate) fn check_and_maybe_rotate(sink: &mut Sink) -> io::Result<()> {
    let meta = match std::fs::metadata(&sink.path) {
        Ok(m) => m,
        Err(_) => {
            // Path vanished (someone unlinked it). Just reopen — the
            // caller's next write lands in a fresh file.
            return sink.reopen();
        }
    };
    if meta.ino() != sink.inode {
        // A sibling process already rotated; pick up the new inode.
        return sink.reopen();
    }
    let need_size = meta.len() >= sink.max_bytes;
    let need_age = sink.rotated_at.elapsed() >= ROTATE_AGE;
    if !need_size && !need_age {
        sink.bytes = meta.len();
        return Ok(());
    }
    rotate_now(sink)
}

fn rotate_now(sink: &mut Sink) -> io::Result<()> {
    let dir = sink.dir.clone();
    let lock_path = dir.join("marspot.log.rotate-lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    let lock_fd = lock_file.as_raw_fd();
    // try_flock(EX | NB) — non-blocking. If a sibling holds it we lose
    // the race; reopen so we pick up whatever inode they're about to
    // create. flock is released automatically on file close, but we
    // unlock explicitly when we own the rotation so the timing is
    // visible in the code.
    let r = unsafe { libc::flock(lock_fd, libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        return sink.reopen();
    }

    // Re-stat under the lock. A sibling may have rotated between our
    // pre-lock trigger and the lock acquire — in which case the file
    // is now small and we should NOT rotate again (would just produce
    // a near-empty rotation).
    let meta = std::fs::metadata(&sink.path)?;
    if meta.ino() != sink.inode {
        unsafe { libc::flock(lock_fd, libc::LOCK_UN) };
        return sink.reopen();
    }
    if meta.len() < sink.max_bytes && sink.rotated_at.elapsed() < ROTATE_AGE {
        unsafe { libc::flock(lock_fd, libc::LOCK_UN) };
        sink.bytes = meta.len();
        return Ok(());
    }

    // Atomic rename to a nanos-stamped name. Nano precision makes
    // collisions across concurrent rotators virtually impossible, and
    // the 20-digit fixed-width formatting keeps lexicographic order
    // matching chronological order — useful for `ls` and for glob
    // patterns used by GC.
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rotated_name = format!("marspot.{:020}.log", stamp);
    let rotated_path = dir.join(&rotated_name);
    std::fs::rename(&sink.path, &rotated_path)?;

    // Open the fresh active file BEFORE releasing the flock so a
    // sibling that grabs the lock next can't observe a window where
    // neither the rotated nor the active file is present.
    let new_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)?;
    let new_meta = new_file.metadata()?;
    sink.file = new_file;
    sink.inode = new_meta.ino();
    sink.bytes = 0;
    sink.rotated_at = Instant::now();
    sink.writes_since_stat = 0;

    unsafe { libc::flock(lock_fd, libc::LOCK_UN) };
    drop(lock_file);

    // Detach compression + retention so the caller's write path returns
    // as soon as the rename + open + assign trio is done. A crash in
    // the compressor just leaves an uncompressed `<ts>.log`; GC sweeps
    // it by age the same as a `.log.gz`.
    let dir_for_thread = dir.clone();
    std::thread::Builder::new()
        .name("marspot-logx-compress".into())
        .spawn(move || {
            let _ = compress_to_gz(&rotated_path);
            prune_retention(&dir_for_thread, env_keep());
        })
        .ok();

    Ok(())
}

#[cfg(feature = "compress")]
fn compress_to_gz(src: &Path) -> io::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::fs::File;
    use std::io::{Read, Write};

    let tmp = path_with_suffix(src, ".gz.tmp");
    let mut f_in = File::open(src)?;
    {
        let f_out = File::create(&tmp)?;
        let mut enc = GzEncoder::new(f_out, Compression::default());
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f_in.read(&mut buf)?;
            if n == 0 {
                break;
            }
            enc.write_all(&buf[..n])?;
        }
        let out = enc.finish()?;
        out.sync_all()?;
    }
    let final_path = path_with_suffix(src, ".gz");
    std::fs::rename(&tmp, &final_path)?;
    std::fs::remove_file(src)?;
    Ok(())
}

#[cfg(not(feature = "compress"))]
fn compress_to_gz(_src: &Path) -> io::Result<()> {
    // Compression opted out (e.g. marspot-session). Leave the rotated
    // `.log` in place; GC's age-based sweep covers it identically.
    Ok(())
}

#[cfg(feature = "compress")]
fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn prune_retention(dir: &Path, keep: usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut backups: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if !s.starts_with("marspot.") {
                return false;
            }
            if s == "marspot.log" {
                return false;
            }
            s.ends_with(".log") || s.ends_with(".log.gz")
        })
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    backups.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    for (path, _) in backups.iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

fn env_keep() -> usize {
    std::env::var("MARSPOT_LOG_KEEP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_KEEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-logx-rot-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn pad(byte: u8, n: usize) -> Vec<u8> {
        let mut v = vec![byte; n];
        // Make the bytes look like a TSV line so the GZ encoder sees
        // realistic input and the snap-to-newline logic is exercised
        // downstream if anyone consumes the rotated file.
        v[n - 1] = b'\n';
        v
    }

    #[test]
    fn rotate_renames_active_at_size_cap_and_creates_fresh() {
        let dir = tmpdir();
        let mut sink = Sink::open_at_dir(dir.clone(), 1024).expect("open sink");
        // Push past 1 KiB so the next write triggers rotate.
        sink.file.write_all(&pad(b'x', 2048)).unwrap();
        sink.bytes = 2048;
        check_and_maybe_rotate(&mut sink).expect("rotate");
        // Active is fresh + empty.
        let active = std::fs::metadata(&sink.path).unwrap();
        assert_eq!(active.len(), 0, "active should be empty after rotate");
        // Some rotated `marspot.<digits>.log` (or `.log.gz` once the
        // background compressor catches up) exists.
        wait_for_rotated(&dir);
    }

    #[test]
    fn rotate_skips_when_under_cap_and_recent() {
        let dir = tmpdir();
        let mut sink = Sink::open_at_dir(dir.clone(), 4096).expect("open sink");
        sink.file.write_all(&pad(b'x', 1000)).unwrap();
        sink.bytes = 1000;
        check_and_maybe_rotate(&mut sink).expect("rotate check");
        // Nothing rotated — active file still holds the data.
        assert_eq!(std::fs::metadata(&sink.path).unwrap().len(), 1000);
        let rotated: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let s = e.file_name();
                let s = s.to_string_lossy();
                s.starts_with("marspot.") && s != "marspot.log" && s != "marspot.log.rotate-lock"
            })
            .collect();
        assert!(rotated.is_empty(), "should not rotate when under cap");
    }

    #[test]
    fn retention_prune_keeps_n_newest_by_mtime() {
        let dir = tmpdir();
        // Plant 5 fake rotated files with distinct mtimes.
        for i in 0..5u32 {
            let path = dir.join(format!("marspot.{:020}.log.gz", i));
            std::fs::write(&path, b"x").unwrap();
            // Stamp mtimes far apart so sort order is unambiguous.
            let secs_ago = (10 - i) as i64 * 60;
            set_mtime_secs_ago(&path, secs_ago);
        }
        prune_retention(&dir, 2);
        let remaining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("marspot."))
            .collect();
        assert_eq!(remaining.len(), 2, "expected 2 to survive, got {:?}", remaining);
    }

    fn wait_for_rotated(dir: &std::path::Path) {
        for _ in 0..50 {
            let any = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| {
                    let n = e.file_name();
                    let s = n.to_string_lossy();
                    s.starts_with("marspot.")
                        && s != "marspot.log"
                        && s != "marspot.log.rotate-lock"
                        && (s.ends_with(".log") || s.ends_with(".log.gz"))
                });
            if any {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("rotated file never appeared in {}", dir.display());
    }

    fn set_mtime_secs_ago(path: &std::path::Path, secs_ago: i64) {
        let cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let target = now - secs_ago;
        let tvs = [
            libc::timeval {
                tv_sec: target as libc::time_t,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: target as libc::time_t,
                tv_usec: 0,
            },
        ];
        let r = unsafe { libc::utimes(cstr.as_ptr(), tvs.as_ptr()) };
        assert_eq!(r, 0, "utimes failed on {}", path.display());
    }
}
