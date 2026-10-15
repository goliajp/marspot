//! Replies to terminal queries that the asker never read.
//!
//! A program asks the terminal something (DA1, XTVERSION, the kitty
//! keyboard flags) and this session answers at once, into the tty's
//! input queue. If the program exits without reading -- claude refusing
//! a resume does exactly this -- the answer is still queued when the
//! shell takes the terminal back, and the shell reads it as typing:
//! zsh took `ESC P` for "search history", recalled the previous command,
//! and appended `>|marspot0u62;1;6;22c` to it.
//!
//! This keeps count of the reply bytes written since anything else was
//! written, watches the queue while the asker still has the terminal,
//! and when the asker is no longer in the foreground and the queue held
//! nothing but those replies, discards them.
//!
//! The queue is measured with `FIONREAD`, which only counts bytes a
//! read would return now. In canonical mode an incomplete line counts as
//! nothing, so a reading is trusted only while the terminal is in raw
//! mode -- which it is while a program that sent a query is running.
//! Measured on macOS: 13 queued bytes read as 13 in raw mode and as 0
//! once the program restored canonical mode, and were all still there.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// Past this the watch is dropped without a verdict; nothing is lost.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

struct Watch {
    /// Reply bytes written since the last write of anything else.
    reply_bytes: usize,
    /// The foreground process group when the first of them was written:
    /// the program that asked.
    pgrp: libc::pid_t,
    /// The last trustworthy reading found nothing in the queue but
    /// replies.
    only_replies: bool,
    since: Instant,
}

#[derive(Default)]
pub struct ReplyGuard {
    watch: Option<Watch>,
    /// Times the input queue was flushed over the session's life.
    flushes: u64,
}

impl ReplyGuard {
    pub fn is_watching(&self) -> bool {
        self.watch.is_some()
    }

    #[cfg(test)]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// `n` reply bytes were queued for the pty.
    pub fn wrote_replies(&mut self, master: RawFd, n: usize) {
        match self.watch.as_mut() {
            Some(w) => w.reply_bytes += n,
            None => {
                let pgrp = unsafe { libc::tcgetpgrp(master) };
                if pgrp <= 0 {
                    return;
                }
                self.watch = Some(Watch { reply_bytes: n, pgrp, only_replies: false, since: Instant::now() });
            }
        }
    }

    /// Something other than a reply was queued: from here on the queue
    /// may hold the person's typing, and none of it is ours to drop.
    pub fn wrote_other(&mut self) {
        self.watch = None;
    }

    /// Look at the queue, and flush it if the replies' asker is gone and
    /// the queue held nothing but replies.
    ///
    /// `delivered` is whether everything queued for the pty has reached
    /// it; until then an empty tty queue means "not arrived yet", not
    /// "read".
    ///
    /// Returns, when it flushed, the reply bytes written since anything
    /// else was: an upper bound on what was discarded. The asker may
    /// have read them in the moment before it left, unseen -- by then
    /// the terminal is usually back in canonical mode and cannot say --
    /// and then the flush found the queue empty. Either way nothing but
    /// replies was in it.
    pub fn check(&mut self, master: RawFd, delivered: bool) -> Option<usize> {
        let w = self.watch.as_mut()?;
        if w.since.elapsed() >= GIVE_UP_AFTER {
            self.watch = None;
            return None;
        }
        let fg = unsafe { libc::tcgetpgrp(master) };
        if fg == w.pgrp {
            if delivered && raw_mode(master) {
                match queued(master) {
                    Some(0) => self.watch = None,
                    Some(q) => w.only_replies = q <= w.reply_bytes,
                    None => {}
                }
            }
            return None;
        }
        let w = self.watch.take()?;
        if !w.only_replies {
            return None;
        }
        flush_input(master)?;
        self.flushes += 1;
        Some(w.reply_bytes)
    }
}

fn raw_mode(master: RawFd) -> bool {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(master, &mut t) == 0 && t.c_lflag & libc::ICANON == 0 }
}

/// The slave side, opened for one look. Not kept: while this process
/// holds the slave open, the master never sees the EIO that says the
/// shell has gone.
struct Slave(RawFd);

impl Slave {
    fn open(master: RawFd) -> Option<Self> {
        let mut name = [0 as libc::c_char; 128];
        if unsafe { libc::ioctl(master, libc::TIOCPTYGNAME as _, name.as_mut_ptr()) } != 0 {
            return None;
        }
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
        (fd >= 0).then_some(Self(fd))
    }
}

impl Drop for Slave {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn queued(master: RawFd) -> Option<usize> {
    let slave = Slave::open(master)?;
    let mut n: libc::c_int = 0;
    (unsafe { libc::ioctl(slave.0, libc::FIONREAD, &mut n) } == 0).then_some(n.max(0) as usize)
}

fn flush_input(master: RawFd) -> Option<()> {
    let slave = Slave::open(master)?;
    (unsafe { libc::tcflush(slave.0, libc::TCIFLUSH) } == 0).then_some(())
}

#[cfg(test)]
mod tests {
    use crate::local_session::LocalSession;
    use std::thread;
    use std::time::{Duration, Instant};

    fn pump_until(s: &mut LocalSession, mut done: impl FnMut(&LocalSession) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            s.pump();
            if done(s) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn screen(s: &LocalSession) -> String {
        let g = s.terminal().grid();
        (0..g.rows())
            .map(|r| (0..g.cols()).map(|c| g.cell(c, r).ch).filter(|&c| c != '\0').collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn shell(id: u64) -> LocalSession {
        let mut s = LocalSession::spawn(id, 100, 30, "", || {}).expect("spawn");
        assert!(pump_until(&mut s, |s| s.terminal().grid().cursor() != (0, 0)), "shell should come up");
        s
    }

    /// The screenshot's case: a program asks, never reads, and exits.
    #[test]
    fn replies_a_program_left_unread_do_not_reach_the_shell() {
        let mut s = shell(0xF1);
        s.write(b"sh -c 'stty raw -echo; printf \"\\033[c\"; sleep 0.5; stty sane'\r").unwrap();
        assert!(pump_until(&mut s, |s| s.reply_flushes() > 0), "the unread reply is flushed");
        s.write(b"IFS= read -r -t 1 x; echo \"<$x>\"\r").unwrap();
        assert!(
            pump_until(&mut s, |s| screen(s).lines().any(|l| l.trim() == "<>")),
            "the next command runs on a clean line:\n{}",
            screen(&s)
        );
        assert!(!screen(&s).contains("62;1;6;22c"), "{}", screen(&s));
    }

    #[test]
    fn a_program_that_reads_its_reply_gets_all_of_it() {
        let mut s = shell(0xF2);
        s.write(
            b"sh -c 'stty raw -echo; printf \"\\033[c\"; r=$(dd bs=13 count=1 2>/dev/null); \
              stty sane; printf \"GOT%s\\n\" \"$(printf %s \"$r\" | tr -d \"\\033\")\"'\r",
        )
        .unwrap();
        assert!(
            pump_until(&mut s, |s| screen(s).contains("GOT[?62;1;6;22c")),
            "the program read its reply:\n{}",
            screen(&s)
        );
        // whether the queue was then flushed (empty) depends on whether
        // the read was seen before the program left; it has its reply
        // either way, which is what matters
        assert!(pump_until(&mut s, |s| !s.watching_replies()));
    }

    /// Typing after the reply puts the person's bytes in the queue too;
    /// none of it may be dropped.
    #[test]
    fn nothing_is_dropped_once_something_else_was_typed() {
        let mut s = shell(0xF3);
        s.write(b"sh -c 'stty raw -echo; printf \"\\033[c\"; sleep 0.5; stty sane'\r").unwrap();
        assert!(pump_until(&mut s, |s| s.watching_replies()), "the reply is being watched");
        s.write(b"echo TYPED\r").unwrap();
        assert!(!s.watching_replies(), "typing ends the watch");
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            s.pump();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.reply_flushes(), 0);
    }
}
