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
//! written and watches the queue while a job holds the terminal. The
//! queue is flushed, when it held nothing but those replies, at the
//! first of two moments the session sees:
//!
//! - the job puts the terminal back in canonical mode (its own `stty
//!   sane`, claude restoring its settings on the way out). A reply has
//!   no newline, so from then on no read returns it.
//! - the job has gone and the shell has the terminal again.
//!
//! The session sees either only when it next looks -- on output, or
//! every 10 ms while watching. A job that restores the terminal and
//! exits within that, under a shell that reads input the moment its job
//! is reaped (bash without prompt hooks), can still hand the shell its
//! replies; waiting on the job's exit with kqueue was tried and lost
//! that race to bash every time. zsh running prompt hooks first does
//! not read that soon.
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
    /// A job -- anything but the shell -- has held the foreground during
    /// the watch.
    ///
    /// Who asked is not recorded, because it cannot be read reliably:
    /// zsh hands the terminal to a new job from its own side of the
    /// fork, and under load the job printed its query before that
    /// happened, so the foreground still named zsh. Measured against
    /// the shell instead, the question is only whether a job has come
    /// and gone.
    job_seen: bool,
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

    /// How soon the session loop should look again, while watching.
    ///
    /// Every millisecond while a job holds replies it may never read:
    /// between the job putting the terminal back in canonical mode and
    /// the next look, the tty can echo the pending reply onto the
    /// screen (seen once in sixty runs at 10 ms). That state lasts only
    /// as long as a program that asked and never read is running, and
    /// at most `GIVE_UP_AFTER`; otherwise every 10 ms.
    pub fn poll_every(&self) -> Option<Duration> {
        let w = self.watch.as_ref()?;
        Some(if w.job_seen && w.only_replies {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(10)
        })
    }

    #[cfg(test)]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// `n` reply bytes were queued for the pty. `empty_before` is
    /// [`queue_was_empty`], asked just before they were.
    ///
    /// An input queue known to be empty when the first reply goes in
    /// holds nothing but replies from then on -- anything else written
    /// ends the watch -- so the verdict is known at once and does not
    /// wait on a reading taken while the asker still has the terminal.
    /// Under heavy load that reading was missed: the asker ran half a
    /// second in raw mode and the session never looked.
    pub fn wrote_replies(&mut self, master: RawFd, shell_pgrp: libc::pid_t, n: usize, empty_before: bool) {
        let w = self.watch.get_or_insert_with(|| Watch {
            reply_bytes: 0,
            job_seen: false,
            only_replies: empty_before,
            since: Instant::now(),
        });
        w.reply_bytes += n;
        let fg = unsafe { libc::tcgetpgrp(master) };
        w.job_seen |= fg > 0 && fg != shell_pgrp;
    }

    /// Something other than a reply was queued: from here on the queue
    /// may hold the person's typing, and none of it is ours to drop.
    pub fn wrote_other(&mut self) {
        self.watch = None;
    }

    /// Look at the queue, and flush it if a job has held the foreground
    /// and given it back to the shell, and the queue held nothing but
    /// replies. A shell's own query -- a prompt asking the terminal
    /// something -- has no job come and go, and is never flushed.
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
    pub fn check(&mut self, master: RawFd, shell_pgrp: libc::pid_t, delivered: bool) -> Option<usize> {
        let w = self.watch.as_mut()?;
        if w.since.elapsed() >= GIVE_UP_AFTER {
            self.watch = None;
            return None;
        }
        let fg = unsafe { libc::tcgetpgrp(master) };
        let shell_in_front = fg == shell_pgrp;
        if !shell_in_front && fg > 0 {
            w.job_seen = true;
            // The job has put the terminal back in canonical mode with
            // the replies still queued. They have no newline, so no read
            // in this mode returns them: nobody will. This is the last
            // moment before the job exits, and the shell, once the job
            // has gone, can read its input before the session looks --
            // bash did, every time, when the flush waited for the exit.
            if delivered && w.only_replies && !raw_mode(master) {
                let w = self.watch.take()?;
                flush_input(master)?;
                self.flushes += 1;
                return Some(w.reply_bytes);
            }
        }
        if !shell_in_front || !w.job_seen {
            // Once the queue is known to hold only replies, it is not
            // read again while a job is in front: FIONREAD on a tty with
            // PENDIN set -- which the kernel sets the moment the job
            // goes back to canonical mode -- re-inputs the pending bytes
            // and echoes them, and a reading that landed in that instant
            // printed the reply on screen. Whether the job then read the
            // replies no longer matters: nothing but replies can be
            // queued, and flushing a queue it emptied costs nothing.
            let settled = w.only_replies && !shell_in_front;
            if delivered && !settled {
                match queued_in_raw_mode(master) {
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

/// Whether the tty's input queue is known to be empty: nothing is still
/// on its way to it, the terminal is in raw mode (in canonical mode an
/// unfinished line reads as nothing), and nothing is waiting to be read.
pub fn queue_was_empty(master: RawFd, delivered: bool) -> bool {
    delivered && queued_in_raw_mode(master) == Some(0)
}

pub fn raw_mode(master: RawFd) -> bool {
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

/// The queue's length, if the terminal was in raw mode both before and
/// after it was read. A program switching to canonical mode in between
/// -- the `stty sane` on its way out -- makes queued bytes read as none,
/// and that reading was once taken for "the replies were read".
fn queued_in_raw_mode(master: RawFd) -> Option<usize> {
    if !raw_mode(master) {
        return None;
    }
    let q = queued(master)?;
    raw_mode(master).then_some(q)
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
        // a loaded runner starts a configured zsh slowly
        let deadline = Instant::now() + Duration::from_secs(30);
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
        shell_named(id, "/bin/zsh")
    }

    /// A pane running `program` as its shell. Each test runs in its own
    /// process, so setting `SHELL` here touches no other test.
    fn shell_named(id: u64, program: &str) -> LocalSession {
        unsafe { std::env::set_var("SHELL", program) };
        let mut s = LocalSession::spawn(id, 100, 30, "", || {}).expect("spawn");
        assert!(pump_until(&mut s, |s| s.terminal().grid().cursor() != (0, 0)), "shell should come up");
        s
    }

    /// The screenshot's case: a program asks, never reads, and exits.
    #[test]
    fn replies_a_program_left_unread_do_not_reach_the_shell() {
        unread_reply_does_not_reach(shell_named(0xF1, "/bin/zsh"));
    }

    /// bash reads input the moment its job is reaped, so the replies
    /// have to be gone before the job exits.
    #[test]
    fn replies_a_program_left_unread_do_not_reach_bash() {
        unread_reply_does_not_reach(shell_named(0xF4, "/bin/bash"));
    }

    fn unread_reply_does_not_reach(mut s: LocalSession) {
        // the job saves the terminal settings, restores them on its way
        // out and is still there for a moment, as claude does. (`stty
        // sane` would set PENDIN, and the tty would echo the pending
        // reply itself at that instant -- on screen, never as input.)
        s.write(b"sh -c 'o=$(stty -g); stty raw -echo; printf \"\\033[c\"; sleep 0.5; stty $o; sleep 0.3'\r").unwrap();
        assert!(pump_until(&mut s, |s| s.watching_replies()), "the query was answered:\n{}", screen(&s));
        assert!(pump_until(&mut s, |s| s.reply_flushes() > 0), "the unread reply is flushed:\n{}", screen(&s));
        // the next keystroke, as a person's would, once the shell is
        // reading input again
        assert!(pump_until(&mut s, |s| s.pty_in_raw_mode()), "the shell reads input again");
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
