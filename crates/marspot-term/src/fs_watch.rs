//! Change notifications for a handful of files, from kqueue.
//!
//! The state files several processes share (settings, the theme and
//! language the system resolves to, a session's registry entry) used to
//! be noticed by `stat` on a timer: once a second, or every five, in
//! every process, whether or not anything had changed.  A terminal with
//! twelve idle panes paid for that with a dozen wake-ups a second that
//! found nothing.
//!
//! A [`Watcher`] blocks in `kevent` instead and calls back when one of
//! its files is created, replaced, written, or removed.  Every writer of
//! these files replaces by rename, which the file's own descriptor
//! cannot report for the *next* file of that name, so the directory is
//! watched too and the files are looked up again when it changes.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What tells one state of a file from the next: which file the name
/// leads to, and when and how much was last written to it.
type Stamp = Option<(u64, i64, i64, u64)>;

fn stamp(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
const CHANGED: u32 = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | GONE;

/// `udata` of a directory's event is its index; a file's has this set.
const FILE_BIT: usize = 1 << 31;
/// `ident` of the user event that ends the thread.
const STOP: libc::uintptr_t = 1;

fn open_evtonly(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn change(kq: &OwnedFd, ident: libc::uintptr_t, filter: i16, flags: u16, fflags: u32, udata: usize) -> io::Result<()> {
    // SAFETY: an all-zero kevent is a valid value.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = ident;
    ev.filter = filter;
    ev.flags = flags;
    ev.fflags = fflags;
    ev.udata = udata as *mut libc::c_void;
    // SAFETY: one change read from `ev`, no events asked for.
    let r = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn arm(kq: &OwnedFd, fd: &OwnedFd, udata: usize) -> io::Result<()> {
    change(kq, fd.as_raw_fd() as libc::uintptr_t, libc::EVFILT_VNODE, libc::EV_ADD | libc::EV_CLEAR, CHANGED, udata)
}

struct Dir {
    path: PathBuf,
    fd: OwnedFd,
}

struct File {
    path: PathBuf,
    dir: usize,
    /// `None` while the file does not exist, and for one that cannot be
    /// opened (a socket): the directory still reports it coming and
    /// going.
    fd: Option<OwnedFd>,
    seen: Stamp,
}

impl File {
    /// Look the name up again and watch what it leads to now.  True if
    /// that differs from what was last seen.  Armed before the look, so
    /// a change after the look is an event.
    fn rearm(&mut self, kq: &OwnedFd, index: usize) -> bool {
        self.fd = open_evtonly(&self.path).ok().filter(|fd| arm(kq, fd, FILE_BIT | index).is_ok());
        let now = stamp(&self.path);
        std::mem::replace(&mut self.seen, now) != now
    }
}

/// Watches files until dropped.
pub struct Watcher {
    kq: Arc<OwnedFd>,
    alive: Arc<AtomicBool>,
}

impl Watcher {
    /// Call `on_change` from a thread of its own each time one of
    /// `files` is created, replaced, written or removed.  Each file's
    /// directory has to exist; the file need not.
    pub fn spawn(files: &[PathBuf], mut on_change: impl FnMut() + Send + 'static) -> io::Result<Watcher> {
        // SAFETY: kqueue returns a new descriptor or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `kq` is a descriptor nothing else owns.
        let kq = Arc::new(unsafe {
            libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
            OwnedFd::from_raw_fd(kq)
        });
        change(&kq, STOP, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, 0)?;
        let mut dirs: Vec<Dir> = Vec::new();
        let mut watched: Vec<File> = Vec::new();
        for path in files {
            let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
            let dir = match dirs.iter().position(|d| d.path == parent) {
                Some(i) => i,
                None => {
                    let fd = open_evtonly(parent)?;
                    arm(&kq, &fd, dirs.len())?;
                    dirs.push(Dir { path: parent.to_path_buf(), fd });
                    dirs.len() - 1
                }
            };
            let mut f = File { path: path.clone(), dir, fd: None, seen: None };
            f.rearm(&kq, watched.len());
            watched.push(f);
        }
        let alive = Arc::new(AtomicBool::new(true));
        let (thread_kq, thread_alive) = (Arc::clone(&kq), Arc::clone(&alive));
        std::thread::Builder::new().name("fs-watch".into()).spawn(move || {
            let kq = thread_kq;
            'wait: loop {
                // SAFETY: an all-zero kevent is a valid value.
                let mut evs: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
                // SAFETY: room for the eight events asked for; no timeout.
                let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, evs.as_mut_ptr(), 8, std::ptr::null()) };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                let mut fire = false;
                for ev in &evs[..n as usize] {
                    if ev.filter == libc::EVFILT_USER {
                        return;
                    }
                    let udata = ev.udata as usize;
                    if udata & FILE_BIT != 0 {
                        let i = udata & !FILE_BIT;
                        // a descriptor replaced since this was queued
                        if watched[i].fd.as_ref().map(|fd| fd.as_raw_fd() as libc::uintptr_t) != Some(ev.ident) {
                            continue;
                        }
                        if ev.fflags & GONE != 0 {
                            watched[i].rearm(&kq, i);
                        } else {
                            watched[i].seen = stamp(&watched[i].path);
                        }
                        fire = true;
                        continue;
                    }
                    if ev.fflags & GONE != 0 {
                        // the directory itself was moved or removed
                        match open_evtonly(&dirs[udata].path) {
                            Ok(fd) if arm(&kq, &fd, udata).is_ok() => dirs[udata].fd = fd,
                            _ => break 'wait,
                        }
                    }
                    for (i, f) in watched.iter_mut().enumerate().filter(|(_, f)| f.dir == udata) {
                        fire |= f.rearm(&kq, i);
                    }
                }
                if fire {
                    on_change();
                }
            }
            // nothing more will be reported: say so, and report once
            // more for whatever the lost directory took with it
            thread_alive.store(false, Ordering::Release);
            on_change();
        })?;
        Ok(Watcher { kq, alive })
    }

    /// False once a watched directory is gone and could not be found
    /// again: changes are no longer reported, and whoever depends on
    /// them has to look for themselves.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // the thread owns the watched descriptors and closes them on its way out
        let _ = change(&self.kq, STOP, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marspot-fs-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn watch(files: &[PathBuf]) -> (Watcher, Receiver<()>) {
        let (tx, rx) = channel();
        let w = Watcher::spawn(files, move || {
            let _ = tx.send(());
        })
        .unwrap();
        (w, rx)
    }

    fn reported(rx: &Receiver<()>) -> bool {
        let got = rx.recv_timeout(Duration::from_secs(3)).is_ok();
        // one change may be seen through both the file and its directory
        std::thread::sleep(Duration::from_millis(50));
        while rx.try_recv().is_ok() {}
        got
    }

    fn quiet(rx: &Receiver<()>) -> bool {
        rx.recv_timeout(Duration::from_millis(300)).is_err()
    }

    #[test]
    fn creation_write_replacement_and_removal_are_each_reported() {
        let d = dir("life");
        let f = d.join("settings.toml");
        let (_w, rx) = watch(std::slice::from_ref(&f));
        assert!(quiet(&rx));

        std::fs::write(&f, "a = 1\n").unwrap();
        assert!(reported(&rx), "creation");

        // written in place
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&f).unwrap().write_all(b"b = 2\n").unwrap();
        assert!(reported(&rx), "write in place");

        // replaced by rename, the way every state file is written; twice,
        // so the second is seen through the descriptor of the first
        for body in ["c = 3\n", "d = 4\n"] {
            std::fs::write(d.join("settings.tmp"), body).unwrap();
            std::fs::rename(d.join("settings.tmp"), &f).unwrap();
            assert!(reported(&rx), "replacement");
        }

        std::fs::remove_file(&f).unwrap();
        assert!(reported(&rx), "removal");
        assert!(quiet(&rx));
    }

    #[test]
    fn a_neighbour_changing_is_not_reported() {
        let d = dir("neighbour");
        let f = d.join("entry.toml");
        std::fs::write(&f, "pid = 1\n").unwrap();
        let (_w, rx) = watch(std::slice::from_ref(&f));
        std::fs::write(d.join("other.bin"), "x").unwrap();
        std::fs::rename(d.join("other.bin"), d.join("other2.bin")).unwrap();
        std::fs::remove_file(d.join("other2.bin")).unwrap();
        assert!(quiet(&rx));
    }

    /// A socket cannot be opened for events; its directory says when it
    /// is unlinked.
    #[test]
    fn a_socket_being_unlinked_is_reported() {
        let d = dir("sock");
        let s = d.join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&s).unwrap();
        let (_w, rx) = watch(std::slice::from_ref(&s));
        assert!(quiet(&rx));
        std::fs::remove_file(&s).unwrap();
        assert!(reported(&rx));
    }

    #[test]
    fn losing_the_directory_is_reported_and_ends_the_watch() {
        let d = dir("lost");
        let f = d.join("entry.toml");
        std::fs::write(&f, "pid = 1\n").unwrap();
        let (w, rx) = watch(std::slice::from_ref(&f));
        assert!(w.alive());
        std::fs::remove_dir_all(&d).unwrap();
        assert!(reported(&rx));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while w.alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!w.alive());
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let d = dir("missing");
        assert!(Watcher::spawn(&[d.join("nowhere").join("f")], || {}).is_err());
    }

    /// Dropping the watcher ends its thread, which is what closes the
    /// descriptors it holds.
    #[test]
    fn dropping_the_watcher_ends_its_thread() {
        let d = dir("drop");
        let held = Arc::new(());
        let in_thread = Arc::clone(&held);
        let w = Watcher::spawn(&[d.join("f")], move || {
            let _ = &in_thread;
        })
        .unwrap();
        assert_eq!(Arc::strong_count(&held), 2);
        drop(w);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while Arc::strong_count(&held) > 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(Arc::strong_count(&held), 1);
    }
}
