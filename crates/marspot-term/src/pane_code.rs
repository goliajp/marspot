//! A pane's four-digit identity code: bound to its session id, kept
//! for as long as that session exists, never shared by two panes.
//!
//! The file `pane-codes` in the state directory holds one
//! `sid<TAB>code` line per pane.  A line that does not read as that is
//! skipped, and anything after the second column is ignored.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const FIRST: u16 = 1000;
pub const LAST: u16 = 9999;
const CODES: usize = (LAST - FIRST + 1) as usize;

/// Holders read a small file, list a directory and rename; this is far
/// above that, and the caller may be a main loop.
const LOCK_TIMEOUT: Duration = Duration::from_millis(500);

/// Nothing is pruned while the file was written this recently: a new
/// pane's session directory is made by its own process a moment after
/// the pane gets its code, and until then the code looks unowned.
const PRUNE_GRACE: Duration = Duration::from_secs(10);

/// The code of pane `sid`, allocated on first ask.
pub fn code_for(sid: u64) -> io::Result<u16> {
    static SEEN: OnceLock<Mutex<HashMap<u64, u16>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some(code) = seen.lock().unwrap().get(&sid) {
        return Ok(*code);
    }
    let path = crate::paths::state_root().join("pane-codes");
    let code = code_in(&path, sid, PRUNE_GRACE, crate::session_registry::list_session_dir_ids, entropy)?;
    seen.lock().unwrap().insert(sid, code);
    Ok(code)
}

fn code_in(path: &Path, sid: u64, grace: Duration, live: impl FnOnce() -> Vec<u64>, rand: impl FnOnce() -> u64) -> io::Result<u16> {
    let known = |rows: &[(u64, u16)]| rows.iter().find(|(s, _)| *s == sid).map(|(_, c)| *c);
    if let Some(code) = known(&read(path)) {
        return Ok(code);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // a file of its own: the table is replaced by rename, and a lock
    // on the replaced file would guard nothing
    let _lock = lock(&path.with_extension("lock"))?;
    let mut rows = read(path);
    if let Some(code) = known(&rows) {
        return Ok(code);
    }
    let settled = fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= grace));
    if settled {
        let live = live();
        rows.retain(|(s, _)| live.contains(s));
    }
    let used: Vec<u16> = rows.iter().map(|(_, c)| *c).collect();
    let code = pick(&used, rand()).ok_or_else(|| io::Error::other("every pane code is taken"))?;
    rows.push((sid, code));
    let text: String = rows.iter().map(|(s, c)| format!("{s}\t{c}\n")).collect();
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(code)
}

/// The rows that read as `sid<TAB>code`; of two rows with one sid or
/// one code, the first.
fn read(path: &Path) -> Vec<(u64, u16)> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut rows: Vec<(u64, u16)> = Vec::new();
    for line in text.lines() {
        let mut cols = line.split('\t');
        let (Some(sid), Some(code)) = (cols.next().and_then(|s| s.parse::<u64>().ok()), cols.next().and_then(|c| c.parse::<u16>().ok())) else {
            continue;
        };
        if (FIRST..=LAST).contains(&code) && !rows.iter().any(|(s, c)| *s == sid || *c == code) {
            rows.push((sid, code));
        }
    }
    rows
}

/// One of the codes not in `used`, each equally likely for a uniform
/// `rand`; None when all are taken.
fn pick(used: &[u16], rand: u64) -> Option<u16> {
    let mut taken = [false; CODES];
    for code in used.iter().filter(|c| (FIRST..=LAST).contains(*c)) {
        taken[usize::from(code - FIRST)] = true;
    }
    let free = taken.iter().filter(|t| !**t).count();
    if free == 0 {
        return None;
    }
    let nth = (rand % free as u64) as usize;
    taken.iter().enumerate().filter(|(_, t)| !**t).nth(nth).map(|(i, _)| FIRST + i as u16)
}

fn lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        // SAFETY: `file` is an open descriptor; the lock goes when it closes
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the pane-codes lock is held"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn entropy() -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: a valid 8-byte buffer; getentropy only fails for a bad
    // pointer or more than 256 bytes
    let rc = unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) };
    assert_eq!(rc, 0, "getentropy failed");
    u64::from_ne_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    const CHILD_ENV: &str = "MARSPOT_PANE_CODE_CHILD";
    const PER_CHILD: u64 = 150;

    fn file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marspot-pane-codes-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("pane-codes")
    }

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Whether `alloc` ever hands out a code that is in the set it was
    /// given, over ten thousand sets of every density.
    fn hands_out_a_taken_code(alloc: impl Fn(&[u16], u64) -> Option<u16>) -> bool {
        let mut s = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..10_000 {
            let density = xorshift(&mut s) % 101;
            let used: Vec<u16> = (FIRST..=LAST).filter(|_| xorshift(&mut s) % 100 < density).collect();
            match alloc(&used, xorshift(&mut s)) {
                Some(code) if used.contains(&code) => return true,
                Some(code) => assert!((FIRST..=LAST).contains(&code)),
                None => assert_eq!(used.len(), CODES),
            }
        }
        false
    }

    #[test]
    fn a_code_in_use_is_never_handed_out() {
        assert!(!hands_out_a_taken_code(pick));
        // the same check catches an allocator that does not look at
        // what is taken
        assert!(hands_out_a_taken_code(|_, r| Some(FIRST + (r % CODES as u64) as u16)));
        let all_but: Vec<u16> = (FIRST..=LAST).filter(|c| *c != 4321).collect();
        for r in [0, 1, 8998, u64::MAX] {
            assert_eq!(pick(&all_but, r), Some(4321));
        }
        let all: Vec<u16> = (FIRST..=LAST).collect();
        assert_eq!(pick(&all, 7), None);
    }

    /// Does nothing unless started by the test below, which runs this
    /// test binary twice at once.
    #[test]
    fn child_allocates() {
        let Ok(spec) = std::env::var(CHILD_ENV) else { return };
        let (path, first) = spec.rsplit_once(':').unwrap();
        let first: u64 = first.parse().unwrap();
        for sid in first..first + PER_CHILD {
            code_in(Path::new(path), sid, PRUNE_GRACE, Vec::new, entropy).unwrap();
        }
    }

    #[test]
    fn two_processes_allocating_at_once_get_different_codes() {
        let path = file("procs");
        let child = |first: u64| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["pane_code::tests::child_allocates", "--exact"])
                .env(CHILD_ENV, format!("{}:{first}", path.display()))
                .spawn()
                .unwrap()
        };
        let mut both = [child(1), child(100_000)];
        for c in &mut both {
            assert!(c.wait().unwrap().success());
        }
        let rows = read(&path);
        let sids: HashSet<u64> = rows.iter().map(|r| r.0).collect();
        let codes: HashSet<u16> = rows.iter().map(|r| r.1).collect();
        assert_eq!(sids.len(), 2 * PER_CHILD as usize, "a row was lost");
        assert_eq!(codes.len(), sids.len(), "two panes share a code");
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), sids.len());
    }

    /// `code_in` keeps nothing in memory, so a second call is what a
    /// restarted process would do.
    #[test]
    fn a_pane_keeps_its_code_across_a_restart() {
        let path = file("restart");
        let first = code_in(&path, 42, PRUNE_GRACE, Vec::new, entropy).unwrap();
        assert!((FIRST..=LAST).contains(&first));
        assert_eq!(code_in(&path, 42, PRUNE_GRACE, Vec::new, entropy).unwrap(), first);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("42\t{first}\n"));
    }

    #[test]
    fn closing_other_panes_changes_no_code_and_frees_theirs() {
        let path = file("prune");
        let code = |sid: u64, grace: Duration, live: &[u64]| code_in(&path, sid, grace, || live.to_vec(), entropy).unwrap();
        let (a, b, c) = (code(1, PRUNE_GRACE, &[]), code(2, PRUNE_GRACE, &[]), code(3, PRUNE_GRACE, &[]));
        // no session directory yet and the file is fresh: all are kept
        assert_eq!(read(&path).len(), 3);
        // pane 2 is closed; the next allocation after the grace drops it
        let d = code(4, Duration::ZERO, &[1, 3]);
        assert_eq!(read(&path), vec![(1, a), (3, c), (4, d)]);
        assert!(d != a && d != c);
        assert_eq!((code(1, Duration::ZERO, &[1, 3, 4]), code(3, Duration::ZERO, &[1, 3, 4])), (a, c));
        let _ = b;
    }

    #[test]
    fn lines_that_do_not_read_are_skipped() {
        let path = file("garbled");
        fs::write(&path, "junk\n7\t12\n8\t1234\tlater-column\n9\t1234\n\n10\t99999\n").unwrap();
        assert_eq!(read(&path), vec![(8, 1234)]);
        let seven = code_in(&path, 7, PRUNE_GRACE, Vec::new, entropy).unwrap();
        assert!((FIRST..=LAST).contains(&seven) && seven != 1234);
        assert_eq!(read(&path), vec![(8, 1234), (7, seven)]);
    }
}
