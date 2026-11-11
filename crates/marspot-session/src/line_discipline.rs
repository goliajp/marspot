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
//!
//! A command can come and go between two looks, so the repair cannot
//! wait to see a job. What the shell's copy holds shows on the terminal
//! twice per line entered: when the shell applies it for the command,
//! and when its line editor starts again from it (zsh's editor leaves
//! `OPOST` and `ISIG` as the copy has them). Flags missing there mean
//! the copy is still damaged -- an empty line or a builtin changes
//! nothing -- and the repair goes on; nothing missing means it is done.
//! A repair that neither sees its job nor reads the copy stops after a
//! moment, or after a few commands, so it can never undo a setting
//! someone asks for later.

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
/// Longest a write to a canonical terminal is held back for the shell to
/// be done with the settings; see `put_back`. For an empty line or a
/// builtin zsh is back in its editor within this of applying its copy,
/// nine times in ten; a command it starts has the terminal after 0.6 ms
/// at the earliest and prints after 2.4 ms (measured).
const SETTLE_FOR: Duration = Duration::from_micros(300);
/// A repair is dropped after this many commands that left the shell's
/// copy damaged: a shell that hands over a terminal already in raw mode
/// (bash, after a program that exited non-zero) never gives it the
/// chance.
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

/// How far the line entered at the prompt has been followed while a
/// repair is pending.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entered {
    /// The shell has not been seen applying its copy yet. Past `until`
    /// a terminal with nothing missing is taken as the shell's doing.
    Waiting { until: Instant },
    /// The shell applied a damaged copy and the flags were put back.
    HandedOver,
}

#[derive(Default)]
pub struct LineGuard {
    kept: Option<Flags>,
    kept_at: Option<Instant>,
    job: Option<Job>,
    /// Flags owed back to the terminal; a repair is pending while set.
    owed: Option<Flags>,
    /// Commands since the repair began that left the shell's copy damaged.
    missed: u8,
    entered: Option<Entered>,
    /// Something other than Enter was typed at the prompt.
    typed: bool,
    /// The shell was last seen in front with `ICANON` off: at a line
    /// editor's prompt.
    edited: bool,
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
    pub fn wrote_input(&mut self, master: RawFd, shell_pgrp: libc::pid_t, bytes: &[u8]) {
        let is_enter = |b: &u8| *b == b'\r' || *b == b'\n';
        if self.owed.is_some() && self.job.is_none() && !bytes.iter().all(is_enter) {
            self.typed = true;
        }
        if !bytes.iter().any(is_enter) {
            return;
        }
        let at = Instant::now();
        self.hot_until = Some(at + HOT_FOR);
        let (Some(owed), None) = (self.owed, &self.job) else { return };
        // a second Enter while the first line's command has the
        // terminal says nothing new about the shell's copy
        if self.entered != Some(Entered::HandedOver) {
            self.entered = Some(Entered::Waiting { until: at + HOT_FOR });
        }
        let give_up = at + HAND_OVER_WAIT;
        while Instant::now() < give_up {
            if let Some(t) = termios(master)
                && t.c_lflag & libc::ICANON != 0
            {
                self.shell_applied(master, shell_pgrp, t, owed, false);
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
        self.look_owed
            || self.hot_until.is_some_and(|t| Instant::now() < t)
            || matches!(self.entered, Some(Entered::Waiting { .. }))
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
            // nothing to follow the line with, and `Waiting` keeps the loop awake
            self.entered = None;
            return;
        }
        self.saw(master, front, shell_pgrp, session_id, at);
    }

    /// `front` is the terminal's foreground group at `at`.
    fn saw(&mut self, master: RawFd, front: libc::pid_t, shell_pgrp: libc::pid_t, session_id: u64, at: Instant) {
        if front != shell_pgrp {
            if self.job.as_ref().is_none_or(|j| j.leader != front) {
                self.watch_exit(front);
                let healed = self.owed.is_some_and(|owed| hand_over(master, shell_pgrp, owed));
                self.job = Some(Job { leader: front, healed });
                self.hot_until = None;
                // the job's end settles the line from here
                self.entered = None;
                self.typed = false;
            }
            return;
        }
        let job = self.job.take();
        let keep_due = self.kept_at.is_none_or(|kept| at.duration_since(kept) >= KEEP_EVERY);
        if job.is_none() && self.owed.is_none() && !keep_due {
            return;
        }
        let Some(t) = termios(master) else { return };
        let now = Flags::of(&t);
        if let Some(job) = job {
            let ending = self.ending_of(job.leader);
            if self.owed.is_some() {
                self.missed += 1;
                if job.healed || self.missed >= GIVE_UP_AFTER_JOBS {
                    self.end_repair();
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
        let canon = t.c_lflag & libc::ICANON != 0;
        // the editor turned out to be back while a write was held back
        let mut back = false;
        if let Some(owed) = self.owed {
            let missing = !owed.missing_from(now).is_empty();
            match self.entered {
                Some(Entered::Waiting { until }) if canon => {
                    back = self.shell_applied(master, shell_pgrp, t, owed, at >= until);
                }
                // a prompt that has just started again shows the shell's copy
                Some(_) if !canon && missing => {
                    put_back(master, shell_pgrp, owed);
                    self.copy_still_damaged();
                }
                Some(Entered::Waiting { until }) if at >= until => self.end_repair(),
                Some(Entered::HandedOver) if !canon => self.end_repair(),
                _ if missing => {
                    back = put_back(master, shell_pgrp, owed) == Some(false) && canon;
                    if back && self.entered.is_some() {
                        self.copy_still_damaged();
                    }
                }
                _ => {}
            }
        }
        self.edited = !canon || back;
        if self.owed.is_none() {
            self.kept = Some(now);
            self.kept_at = Some(at);
        }
    }

    /// The shell is running the line entered, with its copy of the
    /// settings applied. True when the line was done and the editor back
    /// before anything could be written.
    fn shell_applied(
        &mut self,
        master: RawFd,
        shell_pgrp: libc::pid_t,
        t: libc::termios,
        owed: Flags,
        waited: bool,
    ) -> bool {
        if owed.missing_from(Flags::of(&t)).is_empty() {
            if self.edited || waited {
                // after a line editor's prompt, `ICANON` on with nothing
                // missing is the shell's copy; a shell without an editor
                // keeps none, and what was put back has stayed
                self.end_repair();
            }
            return false;
        }
        match put_back(master, shell_pgrp, owed) {
            Some(true) => self.entered = Some(Entered::HandedOver),
            // a builtin or an empty line: the editor started again from
            // the same copy
            Some(false) => {
                self.copy_still_damaged();
                return true;
            }
            None => {}
        }
        false
    }

    /// A line was run and the shell's copy still lacks what is owed.
    fn copy_still_damaged(&mut self) {
        self.entered = None;
        if std::mem::take(&mut self.typed) {
            self.missed += 1;
            if self.missed >= GIVE_UP_AFTER_JOBS {
                self.end_repair();
            }
        }
    }

    fn end_repair(&mut self) {
        self.owed = None;
        self.missed = 0;
        self.entered = None;
        self.typed = false;
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
fn hand_over(master: RawFd, shell_pgrp: libc::pid_t, owed: Flags) -> bool {
    termios(master).is_some_and(|t| owed.missing_from(Flags::of(&t)).is_empty())
        || put_back(master, shell_pgrp, owed) == Some(true)
}

/// Puts `owed` back, and says whether the terminal was canonical when
/// it did.
///
/// Reading the settings and writing them back are two calls, and what
/// the shell writes between them is undone. For a builtin or an empty
/// line zsh writes twice within microseconds -- its copy for the
/// command, then its editor's settings -- and a write made on first
/// sight of the copy left the editor reading a canonical, echoing
/// terminal after one such line in twenty (measured). So a canonical
/// terminal is written to once a job has it, when the shell waits and
/// writes nothing, or after `SETTLE_FOR` -- spent spinning, a command's
/// first output being only a few of those away. If the editor comes back
/// first, the write is made on its settings instead.
///
/// Read and written through the slave side, opened before either so
/// that nothing slow comes between the two: set through the master, the
/// pty driver throws away the output still queued for it, and a prompt
/// the shell had just printed never arrived.
fn put_back(master: RawFd, shell_pgrp: libc::pid_t, owed: Flags) -> Option<bool> {
    let slave = crate::reply_guard::Slave::open(master)?;
    let give_up = Instant::now() + SETTLE_FOR;
    loop {
        // SAFETY: tcgetpgrp on the pty master this session owns.
        let front = unsafe { libc::tcgetpgrp(master) };
        let mut t = termios(slave.fd())?;
        if owed.missing_from(Flags::of(&t)).is_empty() {
            return None;
        }
        let canon = t.c_lflag & libc::ICANON != 0;
        let job = front > 0 && front != shell_pgrp;
        if job && !canon {
            // a job already in raw mode: its settings are its own
            return None;
        }
        if job || !canon || Instant::now() >= give_up {
            t.c_iflag |= owed.input;
            t.c_oflag |= owed.output;
            t.c_lflag |= owed.local;
            // SAFETY: tcsetattr on the slave of the pty this session
            // owns, with the settings just read from it.
            return (unsafe { libc::tcsetattr(slave.fd(), libc::TCSANOW, &t) } == 0).then_some(canon);
        }
        std::hint::spin_loop();
    }
}

/// The settings of the pty this session owns, read from either side.
fn termios(fd: RawFd) -> Option<libc::termios> {
    // SAFETY: an all-zero termios is a valid value for tcgetattr to fill.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr on a descriptor of that pty.
    (unsafe { libc::tcgetattr(fd, &mut t) } == 0).then_some(t)
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

    /// The shell is in its line editor with nothing typed: the cursor
    /// sits after a bare prompt, and the terminal is out of canonical
    /// mode, which both shells' editors see to before they print one.
    fn at_prompt(s: &LocalSession) -> bool {
        let (col, row) = s.terminal().grid().cursor();
        let all = rows(s);
        let text = all[row as usize].trim_end();
        s.pty_in_raw_mode()
            && text.chars().count() >= 2
            && (text.ends_with('%') || text.ends_with('$'))
            && col as usize == text.chars().count() + 1
    }

    /// Type `line` and Enter at the prompt, and wait for the prompt
    /// after it.
    fn run(s: &mut LocalSession, line: &str) {
        let row = s.terminal().grid().cursor().1;
        s.write(format!("{line}\r").as_bytes()).unwrap();
        assert!(
            pump_until(s, |s| s.terminal().grid().cursor().1 > row && at_prompt(s)),
            "the prompt came back after {line:?}:\n{}",
            screen(s)
        );
    }

    fn shell_of(id: u64, rows: u16) -> LocalSession {
        let mut s = LocalSession::spawn(id, 100, rows, "", || {}).expect("spawn");
        assert!(pump_until(&mut s, at_prompt), "shell should come up:\n{}", screen(&s));
        s
    }

    fn shell(id: u64) -> LocalSession {
        shell_of(id, 30)
    }

    /// Kill the raw-mode program and wait for the shell to be back at
    /// its prompt -- and, when `repaired`, for the repair to have begun.
    fn kill_and_wait(s: &mut LocalSession, pid: libc::pid_t, signal: libc::c_int, repaired: bool) {
        // SAFETY: the pid the program in this test's own pane printed.
        unsafe { libc::kill(pid, signal) };
        assert!(
            pump_until(s, |s| at_prompt(s) && (!repaired || s.line_repairs().1)),
            "the shell is back at its prompt:\n{}",
            screen(s)
        );
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
        // bash puts its own settings back, and nothing is repaired
        kill_and_wait(&mut s, pid, signal, shell_path.ends_with("zsh"));
        // the first command after it, as a person would type it
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(
            pump_until(&mut s, |s| rows(s).iter().any(|r| r.trim() == "two")),
            "the command ran:\n{}",
            screen(&s)
        );
        assert!(
            stacked(&s, "one", "two"),
            "newline returns the carriage again, repairs {:?}:\n{}",
            s.line_repairs(),
            screen(&s)
        );
        assert!(
            pump_until(&mut s, |s| !s.line_repairs().1 && at_prompt(s)),
            "the repair ended with that command:\n{}",
            screen(&s)
        );
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
        run(&mut s, "stty -opost -isig");
        assert_eq!(s.pty_flags().1 & libc::OPOST, 0, "stty ran:\n{}", screen(&s));
        run(&mut s, "/usr/bin/true");
        // what is tested: nothing puts the flags back afterwards either
        pump_for(&mut s, Duration::from_millis(300));
        let (_, oflag, lflag) = s.pty_flags();
        assert_eq!(oflag & libc::OPOST, 0, "-opost was asked for");
        assert_eq!(lflag & libc::ISIG, 0, "-isig was asked for");
        assert_eq!(s.line_repairs(), (0, false));
    }

    /// `stty -opost` as the second command after a kill: the repair
    /// ended with the first, and has no say over this.
    #[test]
    fn a_setting_asked_for_after_the_repair_is_not_undone() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell(0xE6);
        let pid = raw_program(&mut s);
        kill_and_wait(&mut s, pid, libc::SIGTERM, true);
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "one", "two")), "the first command ran:\n{}", screen(&s));
        assert!(
            pump_until(&mut s, |s| !s.line_repairs().1 && at_prompt(s)),
            "the repair ended with that command:\n{}",
            screen(&s)
        );
        run(&mut s, "stty -opost");
        assert_eq!(s.pty_flags().1 & libc::OPOST, 0, "stty ran:\n{}", screen(&s));
        run(&mut s, "/usr/bin/true");
        // what is tested: nothing puts the flag back afterwards either
        pump_for(&mut s, Duration::from_millis(300));
        assert_eq!(s.pty_flags().1 & libc::OPOST, 0, "-opost was asked for");
        assert_eq!(s.line_repairs(), (1, false));
    }

    /// A builtin runs no job, so zsh keeps its damaged copy through it:
    /// the repair has to outlast `cd` to reach the command after.
    #[test]
    fn a_builtin_typed_first_does_not_use_the_repair_up() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell(0xE7);
        let pid = raw_program(&mut s);
        kill_and_wait(&mut s, pid, libc::SIGTERM, true);
        run(&mut s, "cd /");
        assert!(s.line_repairs().1, "zsh still holds the damaged settings");
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "one", "two")), "newline returns the carriage:\n{}", screen(&s));
        assert!(pump_until(&mut s, |s| !s.line_repairs().1), "the repair ended with that command");
    }

    /// zsh writes the terminal's settings twice within microseconds for
    /// a line that runs no job. A repair written between the two undid
    /// the second, and the editor was left reading a canonical, echoing
    /// terminal: the next line showed twice and ran unrepaired.
    #[test]
    fn empty_lines_after_a_kill_leave_the_prompt_its_line_editor() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell_of(0xE8, 100);
        let pid = raw_program(&mut s);
        kill_and_wait(&mut s, pid, libc::SIGTERM, true);
        for _ in 0..60 {
            // a person's pace: it took a shell asleep in its read, not
            // one still busy with the last line, to lose its write
            pump_for(&mut s, Duration::from_millis(30));
            run(&mut s, "");
        }
    }

    mod unseen {
        //! The guard driven by hand, in the order a slow machine sees
        //! things: the test plays the shell on a pty nothing runs on.
        use super::super::*;
        use std::process::Command;

        const SHELL: libc::pid_t = 1;

        fn pty() -> (OwnedFd, OwnedFd) {
            let (mut master, mut slave) = (0, 0);
            // SAFETY: openpty fills the two descriptors; no name, settings or size asked for.
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, 0);
            // SAFETY: both were just opened and nothing else owns them.
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
        }

        /// What the shell or a program would set: `canon` is off at a
        /// line editor's prompt, `processed` off is what a raw-mode
        /// program leaves.
        fn put(master: RawFd, canon: bool, processed: bool) {
            let mut t = termios(master).unwrap();
            t.c_lflag = if canon { t.c_lflag | libc::ICANON } else { t.c_lflag & !libc::ICANON };
            if processed {
                t.c_oflag |= libc::OPOST | libc::ONLCR;
                t.c_lflag |= libc::ISIG;
            } else {
                t.c_oflag &= !libc::OPOST;
                t.c_lflag &= !libc::ISIG;
            }
            // SAFETY: tcsetattr on the pty this test opened.
            assert_eq!(unsafe { libc::tcsetattr(master, libc::TCSANOW, &t) }, 0);
        }

        fn processed(master: RawFd) -> bool {
            termios(master).unwrap().c_oflag & libc::OPOST != 0
        }

        /// Up to zsh's prompt after a raw-mode program was killed.
        fn after_a_kill(master: RawFd) -> LineGuard {
            let mut g = LineGuard::default();
            put(master, false, true);
            g.saw(master, SHELL, SHELL, 0, Instant::now());
            let mut program = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            let pid = program.id() as libc::pid_t;
            g.saw(master, pid, SHELL, 0, Instant::now());
            put(master, false, false);
            // SAFETY: the child this test just started.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            program.wait().unwrap();
            // zsh's editor starts again from the settings it copied
            g.saw(master, SHELL, SHELL, 0, Instant::now());
            assert!(g.pending() && processed(master), "the repair began");
            g
        }

        /// The shell's next write may be microseconds away, and a write
        /// made across it undoes it.
        #[test]
        fn a_canonical_terminal_is_not_written_to_at_first_sight() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            put(m, true, false);
            let typed = Instant::now();
            g.wrote_input(m, SHELL, b"cd /\r");
            assert!(typed.elapsed() >= SETTLE_FOR, "written after {:?}", typed.elapsed());
            assert!(processed(m), "and written once the shell had its time");
        }

        #[test]
        fn output_the_shell_has_just_written_survives_the_repair() {
            let (m, slave) = pty();
            let m = m.as_raw_fd();
            put(m, false, false);
            let prompt = b"prompt% ";
            // SAFETY: a write to the pty this test opened.
            let n = unsafe { libc::write(slave.as_raw_fd(), prompt.as_ptr().cast(), prompt.len()) };
            assert_eq!(n, prompt.len() as isize);
            assert_eq!(put_back(m, SHELL, ALL), Some(false));
            let mut got = [0u8; 64];
            // SAFETY: the same pty, read into a buffer of that size without
            // waiting for output that is gone.
            let n = unsafe {
                libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK);
                libc::read(m, got.as_mut_ptr().cast(), got.len())
            };
            assert_eq!(&got[..n.max(0) as usize], prompt);
        }

        #[test]
        fn a_command_gone_before_any_look_still_ends_the_repair() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            // the shell applies its damaged copy for the command
            put(m, true, false);
            g.wrote_input(m, SHELL, b"ls\r");
            assert!(processed(m), "the command got the flags");
            // it ran and ended unseen; the shell copied what it left and
            // its editor started again
            put(m, false, true);
            g.saw(m, SHELL, SHELL, 0, Instant::now());
            assert!(!g.pending(), "the repair ended with that command");
            // `stty -opost`, also gone before any look
            put(m, false, false);
            g.saw(m, SHELL, SHELL, 0, Instant::now() + KEEP_EVERY);
            g.saw(m, SHELL, SHELL, 0, Instant::now() + 2 * KEEP_EVERY);
            assert!(!processed(m), "-opost was asked for");
            assert_eq!(g.repairs(), 1);
        }

        #[test]
        fn a_line_never_seen_being_run_ends_the_repair_after_a_moment() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            // a look timed before the line: however late this thread runs,
            // it is within the moment the shell is given
            let typed = Instant::now();
            g.wrote_input(m, SHELL, b"ls\r");
            g.saw(m, SHELL, SHELL, 0, typed);
            assert!(g.pending(), "the shell may not have read the line yet");
            g.saw(m, SHELL, SHELL, 0, Instant::now() + HOT_FOR);
            assert!(!g.pending());
        }

        #[test]
        fn lines_that_leave_the_copy_damaged_are_counted_and_empty_ones_are_not() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            let line = |g: &mut LineGuard, typed: &[u8]| {
                put(m, true, false);
                g.wrote_input(m, SHELL, typed);
                // back at the prompt, from the same damaged copy
                put(m, false, false);
                g.saw(m, SHELL, SHELL, 0, Instant::now());
                assert!(processed(m), "the prompt is repaired each time");
            };
            for _ in 0..5 {
                line(&mut g, b"\r");
            }
            assert!(g.pending(), "Enter alone runs nothing");
            line(&mut g, b"cd /\r");
            line(&mut g, b"cd /\r");
            assert!(g.pending());
            line(&mut g, b"cd /\r");
            assert!(!g.pending(), "three commands and it is over");
        }
    }

    /// A program that exits non-zero with the terminal raw crashed, but
    /// nothing says its other settings were not meant: only the output
    /// side is put back.
    #[test]
    fn a_failed_exit_gets_only_its_output_flags_back() {
        let _env = test_pane_env("/bin/zsh");
        let mut s = shell(0xE5);
        // it draws, as a full-screen program does: a job that never
        // prints is only looked for in the moment after Enter
        s.write(b"sh -c 'stty raw -echo; printf x; sleep 0.2; exit 3'\r").unwrap();
        assert!(
            pump_until(&mut s, |s| s.line_repairs().0 == 1 && at_prompt(s)),
            "the repair started:\n{}",
            screen(&s)
        );
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "one", "two")), "output is processed again:\n{}", screen(&s));
        assert_eq!(s.pty_flags().2 & libc::ISIG, 0, "the signal flag is not ours to guess");
    }
}
