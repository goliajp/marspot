//! Putting the tty back after a program died holding it in raw mode.
//!
//! A full-screen program clears `OPOST`, `ISIG`, `ICRNL` and the rest,
//! and puts them back on its way out. Killed by a signal it never gets
//! to, and zsh takes whatever the terminal holds when its job ends as
//! the settings for every command after it (`gettyinfo(&shttyinfo)` in
//! `update_job`, whatever the exit status): `ls` then staircases across
//! the pane and Ctrl-C does nothing until someone types `stty sane`.
//! bash restores its own copy when a job dies by a signal; zsh only
//! puts `ICANON` and `ECHO` back.
//!
//! The session sees the tty from the master side, when it looks: on
//! output, and every millisecond for a moment after Enter. It keeps the flags the terminal had while the
//! shell was last in front, and learns how each foreground job ended
//! from kqueue (`NOTE_EXITSTATUS`, for the job's group leader). When
//! the shell is back in front with flags missing:
//!
//! - the leader exited 0: the change was asked for (`stty -opost`), and
//!   is left alone;
//! - the leader died by a signal: the output, signal and newline flags
//!   the terminal had before are owed back;
//! - the leader exited non-zero: only `OPOST` and `ONLCR` are;
//! - its ending could not be read: nothing is done. That is a program
//!   so short it was gone before it could be watched (`stty` itself,
//!   often), a stopped job, or one this user may not signal.
//!
//! `ICANON` and `ECHO` are never touched: a shell with a line editor
//! runs with them off at its prompt and on for its commands, and looks
//! after both itself.
//!
//! Setting the flags once is not enough, because the shell has already
//! copied the damaged settings and applies them again each time it
//! leaves its line editor. So what is owed is put back whenever the
//! shell is in front, and once more when the shell hands the terminal
//! to its next command -- recognisable because the shell's copy has
//! `ICANON` on, which a raw-mode program's never has. That command ends
//! with the flags in place, the shell copies them, and the repair is
//! over. A job that is already in raw mode when first seen is never
//! touched.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

/// How long after Enter the loop looks every millisecond. zsh applies
/// its copy of the settings about 0.4 ms after reading the line and
/// the command it starts needs a few more to print (measured).
const HOT_FOR: Duration = Duration::from_millis(30);
const HOT_POLL: Duration = Duration::from_millis(1);
/// Longest the session waits, on Enter with a repair pending, for the
/// shell to hand the terminal over.
const HAND_OVER_WAIT: Duration = Duration::from_millis(3);
/// A repair is dropped after this many jobs it could not get ahead of:
/// a shell that hands over a terminal already in raw mode (bash, after
/// a program that exited non-zero) never gives it the chance.
const GIVE_UP_AFTER_JOBS: u8 = 3;
/// The flags are re-read at most this often while the shell is in front.
const KEEP_EVERY: Duration = Duration::from_millis(100);
/// Outside the moment after Enter, the terminal is looked at no more
/// often than this however fast output arrives; a look that was put
/// off is made up on the loop's next wake.
const LOOK_EVERY: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Flags {
    input: libc::tcflag_t,
    output: libc::tcflag_t,
    local: libc::tcflag_t,
}

const OUTPUT: Flags = Flags { input: 0, output: libc::OPOST | libc::ONLCR, local: 0 };
const ALL: Flags = Flags {
    input: libc::ICRNL | libc::BRKINT,
    output: libc::OPOST | libc::ONLCR,
    local: libc::ISIG | libc::IEXTEN,
};

impl Flags {
    fn of(t: &libc::termios) -> Self {
        Self { input: t.c_iflag, output: t.c_oflag, local: t.c_lflag }
    }

    fn and(self, o: Self) -> Self {
        Self { input: self.input & o.input, output: self.output & o.output, local: self.local & o.local }
    }

    /// Those of `self` that `now` does not have.
    fn missing_from(self, now: Self) -> Self {
        Self { input: self.input & !now.input, output: self.output & !now.output, local: self.local & !now.local }
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ending {
    Clean,
    Signalled,
    Failed,
    Unknown,
}

struct Job {
    leader: libc::pid_t,
    /// It was given the terminal with everything owed in place, so the
    /// shell copies good settings when it ends.
    healed: bool,
}

#[derive(Default)]
pub struct LineGuard {
    kept: Option<Flags>,
    kept_at: Option<Instant>,
    job: Option<Job>,
    /// Flags owed back to the terminal; a repair is pending while set.
    owed: Option<Flags>,
    /// Jobs that ended since the repair began without being healed.
    missed: u8,
    hot_until: Option<Instant>,
    last_look: Option<Instant>,
    /// A look was put off by `LOOK_EVERY` and is still owed.
    look_owed: bool,
    /// A kqueue the foreground leaders' exits arrive on.
    exits: Option<OwnedFd>,
    repairs: u64,
}

impl LineGuard {
    /// Input was queued for the pty. After Enter the loop looks often
    /// for a moment, so a job that never prints is still seen in front.
    ///
    /// While a repair is pending and the shell is in front, Enter is
    /// also when the shell applies its damaged copy again, and the
    /// command it starts prints a few milliseconds later. Waiting for
    /// the loop's next wake lost that race under load, so this waits
    /// here, briefly, for the shell's copy to appear.
    pub fn wrote_input(&mut self, master: RawFd, bytes: &[u8]) {
        if !bytes.iter().any(|&b| b == b'\r' || b == b'\n') {
            return;
        }
        self.hot_until = Some(Instant::now() + HOT_FOR);
        let (Some(owed), None) = (self.owed, &self.job) else { return };
        let give_up = Instant::now() + HAND_OVER_WAIT;
        while Instant::now() < give_up {
            if let Some(mut t) = termios(master)
                && t.c_lflag & libc::ICANON != 0
            {
                if !owed.missing_from(Flags::of(&t)).is_empty() {
                    set(master, &mut t, owed);
                }
                return;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// How soon the session loop should look again.
    pub fn poll_every(&self) -> Option<Duration> {
        self.wants_look().then_some(HOT_POLL)
    }

    /// Whether to look even though no output arrived.
    pub fn wants_look(&self) -> bool {
        self.look_owed || self.hot_until.is_some_and(|t| Instant::now() < t)
    }

    #[cfg(test)]
    pub fn repairs(&self) -> u64 {
        self.repairs
    }

    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.owed.is_some()
    }

    pub fn look(&mut self, master: RawFd, shell_pgrp: libc::pid_t, session_id: u64) {
        let at = Instant::now();
        let hot = self.hot_until.is_some_and(|t| at < t);
        if !hot && self.last_look.is_some_and(|t| at.duration_since(t) < LOOK_EVERY) {
            self.look_owed = true;
            return;
        }
        self.look_owed = false;
        self.last_look = Some(at);
        // SAFETY: tcgetpgrp on the pty master this session owns.
        let front = unsafe { libc::tcgetpgrp(master) };
        if front <= 0 {
            return;
        }
        if front != shell_pgrp {
            if self.job.as_ref().is_none_or(|j| j.leader != front) {
                self.watch_exit(front);
                let healed = self.owed.is_some_and(|owed| hand_over(master, owed));
                self.job = Some(Job { leader: front, healed });
                self.hot_until = None;
            }
            return;
        }
        let job = self.job.take();
        let keep_due = self.kept_at.is_none_or(|kept| at.duration_since(kept) >= KEEP_EVERY);
        if job.is_none() && self.owed.is_none() && !keep_due {
            return;
        }
        let Some(mut t) = termios(master) else { return };
        let now = Flags::of(&t);
        if let Some(job) = job {
            let ending = self.ending_of(job.leader);
            if self.owed.is_some() {
                self.missed += 1;
                if job.healed || self.missed >= GIVE_UP_AFTER_JOBS {
                    self.owed = None;
                    self.missed = 0;
                }
            }
            if self.owed.is_none()
                && let Some(kept) = self.kept
            {
                let owed = match ending {
                    Ending::Clean | Ending::Unknown => Flags::default(),
                    Ending::Signalled => kept.and(ALL),
                    Ending::Failed => kept.and(OUTPUT),
                };
                let lost = owed.missing_from(now);
                if !lost.is_empty() {
                    self.owed = Some(owed);
                    self.repairs += 1;
                    marspot_term::lx_info!(
                        "l3.tty_repair",
                        "the foreground program ended without putting the terminal's \
                         settings back; restoring them",
                        session_id = session_id,
                        ending = format!("{ending:?}"),
                        oflag_lost = format!("{:#x}", lost.output),
                        lflag_lost = format!("{:#x}", lost.local)
                    );
                }
            }
        }
        match self.owed {
            Some(owed) => {
                if !owed.missing_from(now).is_empty() {
                    set(master, &mut t, owed);
                }
            }
            None => {
                self.kept = Some(now);
                self.kept_at = Some(at);
            }
        }
    }

    fn watch_exit(&mut self, pid: libc::pid_t) {
        if self.exits.is_none() {
            // SAFETY: kqueue returns a new descriptor or -1.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return;
            }
            // SAFETY: `kq` is a descriptor nothing else owns, and from
            // here on this field does.
            self.exits = Some(unsafe {
                libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
                OwnedFd::from_raw_fd(kq)
            });
        }
        let Some(kq) = self.exits.as_ref() else { return };
        // SAFETY: an all-zero kevent is a valid value.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = pid as libc::uintptr_t;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT | libc::NOTE_EXITSTATUS;
        // refused for a process this user may not signal, or one
        // already gone: its ending then reads as unknown
        // SAFETY: one change, no events asked for.
        unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    /// How `leader` ended, from the exits queued so far. The exit is
    /// queued before the shell can take the terminal back: the shell
    /// only does that after reaping it.
    fn ending_of(&mut self, leader: libc::pid_t) -> Ending {
        let Some(kq) = self.exits.as_ref() else { return Ending::Unknown };
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut ending = Ending::Unknown;
        loop {
            // SAFETY: an all-zero kevent is a valid value.
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: room for the one event asked for, and a zero timeout.
            let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, &zero) };
            if n != 1 {
                return ending;
            }
            if ev.ident == leader as libc::uintptr_t {
                let status = ev.data as libc::c_int;
                ending = if libc::WIFSIGNALED(status) {
                    Ending::Signalled
                } else if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    Ending::Clean
                } else {
                    Ending::Failed
                };
            }
        }
    }
}

/// A job has just been seen in front while flags are owed. True when it
/// has the terminal with all of them: either they were already there,
/// or the terminal is as the shell left it for a command (`ICANON` on)
/// and they are put back now. A job already in raw mode is left alone.
fn hand_over(master: RawFd, owed: Flags) -> bool {
    let Some(mut t) = termios(master) else { return false };
    if owed.missing_from(Flags::of(&t)).is_empty() {
        return true;
    }
    t.c_lflag & libc::ICANON != 0 && set(master, &mut t, owed)
}

fn termios(master: RawFd) -> Option<libc::termios> {
    // SAFETY: an all-zero termios is a valid value for tcgetattr to fill.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr on the pty master this session owns.
    (unsafe { libc::tcgetattr(master, &mut t) } == 0).then_some(t)
}

fn set(master: RawFd, t: &mut libc::termios, flags: Flags) -> bool {
    t.c_iflag |= flags.input;
    t.c_oflag |= flags.output;
    t.c_lflag |= flags.local;
    // SAFETY: tcsetattr on the pty master this session owns, with the
    // settings just read from it.
    unsafe { libc::tcsetattr(master, libc::TCSANOW, t) == 0 }
}

#[cfg(test)]
mod tests {
    use crate::local_session::{LocalSession, test_pane_env};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Pump the way the session loop does: as often as the session asks.
    fn pump_until(s: &mut LocalSession, mut done: impl FnMut(&LocalSession) -> bool) -> bool {
        // a loaded runner starts a configured zsh slowly
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            s.pump();
            if done(s) {
                return true;
            }
            thread::sleep(s.reply_poll_every().unwrap_or(Duration::from_millis(5)));
        }
        false
    }

    fn pump_for(s: &mut LocalSession, d: Duration) {
        let end = Instant::now() + d;
        pump_until(s, |_| Instant::now() >= end);
    }

    /// Each row as it sits on screen, columns kept.
    fn rows(s: &LocalSession) -> Vec<String> {
        let g = s.terminal().grid();
        (0..g.rows())
            .map(|r| (0..g.cols()).map(|c| g.cell(c, r).ch).map(|c| if c == '\0' { ' ' } else { c }).collect())
            .collect()
    }

    fn screen(s: &LocalSession) -> String {
        rows(s).iter().map(|r| r.trim_end()).collect::<Vec<_>>().join("\n")
    }

    /// `first` on one row and `second` on the next, both from column 0:
    /// what two lines of output look like when newline still returns
    /// the carriage.
    fn stacked(s: &LocalSession, first: &str, second: &str) -> bool {
        rows(s).windows(2).any(|w| w[0].trim_end() == first && w[1].trim_end() == second)
    }

    fn shell(id: u64) -> LocalSession {
        let mut s = LocalSession::spawn(id, 100, 30, "", || {}).expect("spawn");
        assert!(pump_until(&mut s, |s| s.terminal().grid().cursor() != (0, 0)), "shell should come up");
        s
    }

    /// Start a program that puts the terminal in raw mode and stays,
    /// as a full-screen program does. Returns its pid.
    fn raw_program(s: &mut LocalSession) -> libc::pid_t {
        s.write(b"sh -c 'stty raw -echo; printf \"UP%s.\" $$; exec sleep 30'\r").unwrap();
        let mut pid = 0;
        assert!(
            pump_until(s, |s| {
                let text = screen(s);
                let Some(at) = text.rfind("UP") else { return false };
                let digits: String = text[at + 2..].chars().take_while(char::is_ascii_digit).collect();
                if !text[at + 2 + digits.len()..].starts_with('.') {
                    return false;
                }
                pid = digits.parse().unwrap_or(0);
                pid > 0
            }),
            "the raw-mode program came up:\n{}",
            screen(s)
        );
        pid
    }

    fn killed_in_raw_mode(shell_path: &str, id: u64, signal: libc::c_int) {
        let _env = test_pane_env(shell_path);
        let mut s = shell(id);
        let pid = raw_program(&mut s);
        assert_eq!(s.pty_flags().1 & libc::OPOST, 0, "the program has the terminal raw");
        // SAFETY: the pid the program in this test's own pane printed.
        unsafe { libc::kill(pid, signal) };
        // long enough for the shell to be back at its prompt
        pump_for(&mut s, Duration::from_millis(700));
        // the first command after it, as a person would type it
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(
            pump_until(&mut s, |s| rows(s).iter().any(|r| r.trim() == "two")),
            "the command ran:\n{}",
            screen(&s)
        );
        assert!(stacked(&s, "one", "two"), "newline returns the carriage again:\n{}", screen(&s));
        assert!(pump_until(&mut s, |s| !s.line_repairs().1), "the repair ended with that command");
        // and it holds without the session doing anything more
        s.write(b"/usr/bin/printf 'three\\nfour\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "three", "four")), "later commands too:\n{}", screen(&s));
        let (_, oflag, lflag) = s.pty_flags();
        assert_ne!(oflag & libc::OPOST, 0);
        assert_ne!(lflag & libc::ISIG, 0, "Ctrl-C interrupts again");
    }

    /// The report: codex killed by SIGTERM, `ls` staircasing afterwards.
    #[test]
    fn a_raw_program_killed_by_sigterm_does_not_leave_zsh_staircasing() {
        killed_in_raw_mode("/bin/zsh", 0xE1, libc::SIGTERM);
    }

    #[test]
    fn a_raw_program_killed_by_sigkill_does_not_leave_zsh_staircasing() {
        killed_in_raw_mode("/bin/zsh", 0xE2, libc::SIGKILL);
    }

    /// bash puts its own settings back when a job dies by a signal;
    /// nothing here may get in its way.
    #[test]
    fn a_raw_program_killed_under_bash_leaves_the_pane_as_bash_restores_it() {
        killed_in_raw_mode("/bin/bash", 0xE3, libc::SIGTERM);
    }

    /// A setting someone asked for stays: `stty` exits 0.
    #[test]
    fn settings_changed_on_purpose_are_left_alone() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell(0xE4);
        s.write(b"stty -opost -isig\r").unwrap();
        assert!(pump_until(&mut s, |s| s.pty_flags().1 & libc::OPOST == 0), "stty ran:\n{}", screen(&s));
        pump_for(&mut s, Duration::from_millis(500));
        s.write(b"/usr/bin/true\r").unwrap();
        pump_for(&mut s, Duration::from_millis(500));
        let (_, oflag, lflag) = s.pty_flags();
        assert_eq!(oflag & libc::OPOST, 0, "-opost was asked for");
        assert_eq!(lflag & libc::ISIG, 0, "-isig was asked for");
        assert_eq!(s.line_repairs(), (0, false));
    }

    /// A program that exits non-zero with the terminal raw crashed, but
    /// nothing says its other settings were not meant: only the output
    /// side is put back.
    #[test]
    fn a_failed_exit_gets_only_its_output_flags_back() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell(0xE5);
        s.write(b"sh -c 'stty raw -echo; sleep 0.2; exit 3'\r").unwrap();
        assert!(pump_until(&mut s, |s| s.line_repairs().0 == 1), "the repair started:\n{}", screen(&s));
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "one", "two")), "output is processed again:\n{}", screen(&s));
        assert_eq!(s.pty_flags().2 & libc::ISIG, 0, "the signal flag is not ours to guess");
    }
}
