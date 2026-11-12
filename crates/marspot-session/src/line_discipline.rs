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
//! A line that shows neither -- the shell has not got to it yet, or it
//! keeps no copy -- concludes nothing, however long that lasts; such
//! lines are counted when the next one is entered, damaged copies are
//! counted too, and a repair stops after a few, so it can never undo a
//! setting someone asks for later.
//!
//! Nothing above decides what the first command's output looks like:
//! the session cannot be told when the shell writes the settings, only
//! look, and a command can print before any look. So while output
//! flags are owed, output is put right as it is read instead: a line
//! feed gets its carriage return unless a program that chose raw mode
//! has the terminal (`output_is_raw`). Doing that to output the
//! terminal had already processed changes nothing.
//!
//! bash keeps no copy across commands: readline saves what the terminal
//! holds when its prompt starts -- before any repair of that prompt --
//! and puts that back on Enter, raw mode and all, for the command. Its
//! flags go back while the shell is still in front, or while the
//! command's process is still the shell's own image and cannot have
//! asked for anything. A command first seen in raw mode after its exec
//! may have asked for it and is left alone, output included: under bash
//! a first command that is still running when its output is read, and
//! was never seen before its exec, staircases until it ends.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use exits::Ending;
use flags::{ALL, Flags, OUTPUT};

mod exits;
mod flags;

/// How long the loop looks every millisecond after Enter, and after the
/// shell read the line. zsh applies its copy of the settings about 0.4 ms
/// after reading the line and the command it starts needs a few more to
/// print (measured).
const HOT_FOR: Duration = Duration::from_millis(30);
const HOT_POLL: Duration = Duration::from_millis(1);
/// How long Enter watches for the shell's hand-over before going back
/// to the loop. Only the shell's copy heals sooner for it; what the
/// command prints is right either way.
const ENTER_WATCH: Duration = Duration::from_millis(3);
/// How long after Enter the loop keeps looking every millisecond for a
/// line whose outcome has not shown. Past it the repair stays pending
/// and is taken up again by the next output.
const WAIT_POLL_FOR: Duration = Duration::from_secs(2);
/// A repair is dropped after this many commands that left the shell's
/// copy damaged: a shell whose copy never heals must not keep the
/// repair for good.
const GIVE_UP_AFTER_JOBS: u8 = 3;
/// The flags are re-read at most this often while the shell is in front.
const KEEP_EVERY: Duration = Duration::from_millis(100);
/// Outside the moment after Enter, the terminal is looked at no more
/// often than this however fast output arrives; a look that was put
/// off is made up on the loop's next wake.
const LOOK_EVERY: Duration = Duration::from_millis(1);

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
    /// The shell has not been seen applying its copy yet; `read` once
    /// the line was seen gone from the terminal's input queue.
    Waiting { since: Instant, read: bool },
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
    /// Input queued for the pty had not all reached it at the last look.
    undelivered: bool,
    hot_until: Option<Instant>,
    last_look: Option<Instant>,
    /// A look was put off by `LOOK_EVERY` and is still owed.
    look_owed: bool,
    exits: exits::Exits,
    repairs: u64,
}

impl LineGuard {
    /// Input was queued for the pty. After Enter the loop looks often
    /// for a moment, so a job that never prints is still seen in front.
    ///
    /// While a repair is pending and the shell is in front, Enter is
    /// also when the shell applies its damaged copy again, so this
    /// watches for it briefly.
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
        if matches!(self.entered, Some(Entered::Waiting { read: true, .. })) {
            // the last line was read and showed nothing of a copy
            self.missed += 1;
            if self.missed >= GIVE_UP_AFTER_JOBS {
                self.end_repair();
                return;
            }
        }
        self.entered = Some(Entered::Waiting { since: at, read: false });
        while at.elapsed() < ENTER_WATCH {
            // SAFETY: tcgetpgrp on the pty master this session owns.
            let front = unsafe { libc::tcgetpgrp(master) };
            if front > 0 && front != shell_pgrp {
                self.saw(master, front, shell_pgrp, 0, Instant::now());
                return;
            }
            let Some(t) = termios(master) else { return };
            let canon = t.c_lflag & libc::ICANON != 0;
            let missing = !owed.missing_from(Flags::of(&t)).is_empty();
            if canon && !missing {
                self.end_repair();
                return;
            }
            if !canon && missing {
                // readline put back what it saved, or zsh's editor is
                // back already from a damaged copy
                put_back(master, shell_pgrp, owed, false);
                self.copy_still_damaged();
                return;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// Whether output read now may have been written without the output
    /// processing that is owed, and is to get it here: flags are owed,
    /// and no program that chose raw mode has the terminal.
    pub fn output_is_raw(&self, master: RawFd, shell_pgrp: libc::pid_t) -> bool {
        if self.owed.is_none_or(|owed| owed.output == 0) {
            return false;
        }
        // SAFETY: tcgetpgrp on the pty master this session owns.
        let front = || unsafe { libc::tcgetpgrp(master) };
        let job = front();
        if job <= 0 || job == shell_pgrp {
            return true;
        }
        let canon = termios(master).is_none_or(|t| t.c_lflag & libc::ICANON != 0);
        !chose_raw(job, canon, unstarted(job, shell_pgrp), front())
    }

    /// How soon the session loop should look again.
    pub fn poll_every(&self) -> Option<Duration> {
        self.wants_look().then_some(HOT_POLL)
    }

    /// Whether to look even though no output arrived.
    pub fn wants_look(&self) -> bool {
        self.look_owed
            || self.hot_until.is_some_and(|t| Instant::now() < t)
            || matches!(self.entered, Some(Entered::Waiting { since, .. }) if since.elapsed() < WAIT_POLL_FOR)
    }

    #[cfg(test)]
    pub fn repairs(&self) -> u64 {
        self.repairs
    }

    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.owed.is_some()
    }

    pub fn look(&mut self, master: RawFd, shell_pgrp: libc::pid_t, session_id: u64, delivered: bool) {
        let at = Instant::now();
        self.undelivered = !delivered;
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
                self.exits.watch(front);
                let healed = self.owed.is_some_and(|owed| hand_over(master, shell_pgrp, front, owed));
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
            let ending = self.exits.ending_of(job.leader);
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
        if let Some(owed) = self.owed {
            let missing = !owed.missing_from(now).is_empty();
            match self.entered {
                // after a line editor's prompt, `ICANON` on with nothing
                // missing is the shell's copy; a shell without an editor
                // keeps none, and what was put back has stayed
                Some(Entered::Waiting { .. }) if canon && !missing => self.end_repair(),
                // a prompt that has just started again shows the shell's copy
                Some(_) if !canon && missing => {
                    put_back(master, shell_pgrp, owed, false);
                    self.copy_still_damaged();
                }
                Some(Entered::Waiting { since, read: false })
                    if crate::reply_guard::queue_was_empty(master, !self.undelivered) =>
                {
                    self.entered = Some(Entered::Waiting { since, read: true });
                }
                _ if missing => {
                    put_back(master, shell_pgrp, owed, false);
                }
                _ => {}
            }
        }
        if self.owed.is_none() {
            self.kept = Some(now);
            self.kept_at = Some(at);
        }
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
}

/// Whether the job seen in front has the terminal in a raw mode of its
/// own choosing: not canonical, past its exec, and still in front once
/// the settings had been read.
///
/// Who is in front and what the settings are take two calls.  A command
/// that ended between them left its name on the first and the shell's
/// line editor -- never canonical -- on the second, which read as a
/// program that had asked for raw mode: its output, written under the
/// damaged settings, was passed on without its carriage returns.
fn chose_raw(job: libc::pid_t, canon: bool, unstarted: bool, front_after: libc::pid_t) -> bool {
    !canon && !unstarted && front_after == job
}

/// A job has just been seen in front while flags are owed. True when it
/// has the terminal with all of them: either they were already there,
/// or the terminal is as the shell left it for a command and they are
/// put back now. That is `ICANON` on, or a leader still running the
/// shell's own image: it has not exec'd, so raw mode is what the shell
/// handed it (bash). Any other job in raw mode is left alone.
fn hand_over(master: RawFd, shell_pgrp: libc::pid_t, front: libc::pid_t, owed: Flags) -> bool {
    if termios(master).is_some_and(|t| owed.missing_from(Flags::of(&t)).is_empty()) {
        return true;
    }
    let unstarted = unstarted(front, shell_pgrp);
    put_back(master, shell_pgrp, owed, unstarted).is_some_and(|canon| canon || unstarted)
}

/// Whether the job led by `front` still runs the shell's own image.
fn unstarted(front: libc::pid_t, shell_pgrp: libc::pid_t) -> bool {
    use marspot_term::platform::pid_exe_path;
    pid_exe_path(front).is_some_and(|exe| Some(exe) == pid_exe_path(shell_pgrp))
}

/// `bytes` as the terminal's output processing would have passed them
/// on: a carriage return before each line feed that has none.
pub fn with_carriage_returns(bytes: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// Puts `owed` back, and says whether the terminal was canonical when
/// it did.
///
/// Reading the settings and writing them back are two calls, and what
/// the shell writes between them is undone. For a builtin or an empty
/// line zsh writes twice within microseconds -- its copy for the
/// command, then its editor's settings -- and a write made across the
/// second left the editor reading a canonical, echoing terminal. So a
/// canonical terminal is only written to once a job has it, when the
/// shell waits and writes nothing; with the shell in front, only when
/// its editor's settings are on it.
///
/// Read and written through the slave side, opened before either so
/// that nothing slow comes between the two: set through the master, the
/// pty driver throws away the output still queued for it, and a prompt
/// the shell had just printed never arrived.
///
/// `unstarted`: the job in front has not exec'd, and its raw mode is the
/// shell's doing.
fn put_back(master: RawFd, shell_pgrp: libc::pid_t, owed: Flags, unstarted: bool) -> Option<bool> {
    let slave = crate::reply_guard::Slave::open(master)?;
    // SAFETY: tcgetpgrp on the pty master this session owns.
    let front = unsafe { libc::tcgetpgrp(master) };
    let mut t = termios(slave.fd())?;
    let canon = t.c_lflag & libc::ICANON != 0;
    let job = front > 0 && front != shell_pgrp;
    // a job in raw mode after its exec has settings of its own
    if owed.missing_from(Flags::of(&t)).is_empty() || job != canon && !(job && unstarted) {
        return None;
    }
    t.c_iflag |= owed.input;
    t.c_oflag |= owed.output;
    t.c_lflag |= owed.local;
    // SAFETY: tcsetattr on the slave of the pty this session owns, with
    // the settings just read from it.
    (unsafe { libc::tcsetattr(slave.fd(), libc::TCSANOW, &t) } == 0).then_some(canon)
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

    /// The first commands after the kill, as a person would type them,
    /// each printing as it should. A command the session saw holding the
    /// terminal ends the repair; one that came and went between two looks
    /// is counted, and that many end it too.
    fn commands_until_the_repair_is_over(s: &mut LocalSession) {
        for (first, second) in [("one", "two"), ("three", "four"), ("five", "six")] {
            s.write(format!("/usr/bin/printf '{first}\\n{second}\\n'\r").as_bytes()).unwrap();
            assert!(
                pump_until(s, |s| rows(s).iter().any(|r| r.trim() == second) && at_prompt(s)),
                "the command ran:\n{}",
                screen(s)
            );
            assert!(
                stacked(s, first, second),
                "newline returns the carriage again, repairs {:?}:\n{}",
                s.line_repairs(),
                screen(s)
            );
        }
        assert!(pump_until(s, |s| !s.line_repairs().1 && at_prompt(s)), "the repair is over:\n{}", screen(s));
    }

    fn killed_in_raw_mode(shell_path: &str, id: u64, signal: libc::c_int) {
        let _env = test_pane_env(shell_path);
        let mut s = shell(id);
        let pid = raw_program(&mut s);
        assert_eq!(s.pty_flags().1 & libc::OPOST, 0, "the program has the terminal raw");
        // bash puts its own settings back, and nothing is repaired
        kill_and_wait(&mut s, pid, signal, shell_path.ends_with("zsh"));
        commands_until_the_repair_is_over(&mut s);
        // and it holds without the session doing anything more
        s.write(b"/usr/bin/printf 'seven\\neight\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| stacked(s, "seven", "eight")), "later commands too:\n{}", screen(&s));
        let (_, oflag, lflag) = s.pty_flags();
        assert_ne!(oflag & libc::OPOST, 0);
        assert_ne!(lflag & libc::ISIG, 0, "Ctrl-C interrupts again");
    }

    /// A command gone by the time the settings were read is not a
    /// program that chose raw mode, whatever the settings say.
    #[test]
    fn a_job_that_ended_between_the_two_reads_did_not_choose_raw_mode() {
        let (job, shell) = (4242, 4000);
        assert!(super::chose_raw(job, false, false, job), "raw, exec'd, still in front");
        assert!(!super::chose_raw(job, false, false, shell), "the editor's settings under the job's name");
        assert!(!super::chose_raw(job, true, false, job), "canonical is the shell's hand-over");
        assert!(!super::chose_raw(job, false, true, job), "not exec'd: raw is the shell's doing");
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
        commands_until_the_repair_is_over(&mut s);
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
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
        /// made across it undoes it: with the shell in front, a canonical
        /// terminal is left to it.
        #[test]
        fn a_canonical_terminal_is_not_written_to_under_the_shell() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            put(m, true, false);
            g.wrote_input(m, SHELL, b"cd /\r");
            g.saw(m, SHELL, SHELL, 0, Instant::now());
            assert!(!processed(m) && g.pending());
            assert!(g.output_is_raw(m, SHELL), "what a command prints meanwhile is put right as read");
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
            assert_eq!(put_back(m, SHELL, ALL, false), Some(false));
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
        fn a_command_gone_before_any_look_is_counted_and_the_prompt_repaired() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            // the shell applies its damaged copy for the command
            put(m, true, false);
            g.wrote_input(m, SHELL, b"ls\r");
            // it ran and ended unseen; the shell copied what it left and
            // its editor started again
            put(m, false, false);
            g.saw(m, SHELL, SHELL, 0, Instant::now());
            assert!(processed(m) && g.pending());
            assert_eq!(g.missed, 1);
        }

        /// A loaded machine: the shell reads the line long after it was
        /// entered. Until it has, a prompt with nothing missing says
        /// nothing about the copy the shell is about to apply.
        #[test]
        fn a_shell_late_to_read_the_line_still_gets_its_command_repaired() {
            let (m, slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            let line = b"ls\r";
            // SAFETY: writes to and reads from the pty this test opened.
            assert_eq!(unsafe { libc::write(m, line.as_ptr().cast(), line.len()) }, 3);
            g.wrote_input(m, SHELL, line);
            g.saw(m, SHELL, SHELL, 0, Instant::now() + 100 * HOT_FOR);
            assert!(g.pending(), "the line is still unread");
            let mut got = [0u8; 8];
            // SAFETY: as above.
            assert_eq!(unsafe { libc::read(slave.as_raw_fd(), got.as_mut_ptr().cast(), got.len()) }, 3);
            put(m, true, false);
            g.saw(m, SHELL, SHELL, 0, Instant::now() + 101 * HOT_FOR);
            assert!(g.pending() && g.output_is_raw(m, SHELL), "the command's output is still put right");
        }

        /// A shell that keeps no copy never shows one: the lines it read
        /// are counted, and the repair ends with the count, not a clock.
        #[test]
        fn lines_that_show_nothing_end_the_repair_by_their_number() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            for _ in 0..GIVE_UP_AFTER_JOBS {
                assert!(g.pending());
                g.wrote_input(m, SHELL, b"ls\r");
                g.saw(m, SHELL, SHELL, 0, Instant::now() + Duration::from_secs(3600));
            }
            assert!(g.pending(), "the last line may still show a copy");
            g.wrote_input(m, SHELL, b"ls\r");
            assert!(!g.pending());
        }

        /// What the first command printed before any look is put right
        /// as it is read, for as long as the repair lasts.
        #[test]
        fn output_is_given_its_carriage_returns_while_they_are_owed() {
            let (m, _slave) = pty();
            let m = m.as_raw_fd();
            let mut g = after_a_kill(m);
            put(m, true, false);
            assert!(g.output_is_raw(m, SHELL));
            assert_eq!(with_carriage_returns(b"one\ntwo\r\n\n".to_vec()), b"one\r\ntwo\r\n\r\n");
            g.end_repair();
            assert!(!g.output_is_raw(m, SHELL));
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

    /// bash keeps no copy: readline saves the raw settings a failed
    /// program left when its prompt starts, and puts them back for the
    /// command. The flags have to go back between Enter and the command.
    #[test]
    fn a_failed_exit_under_bash_does_not_leave_the_next_command_staircasing() {
        let _env = test_pane_env("/bin/bash");
        let mut s = shell(0xE9);
        s.write(b"sh -c 'stty raw -echo; printf x; sleep 0.2; exit 3'\r").unwrap();
        assert!(
            pump_until(&mut s, |s| s.line_repairs().0 == 1 && at_prompt(s)),
            "the repair started:\n{}",
            screen(&s)
        );
        // the program also left echo off, which is not ours to put back:
        // readline then prints neither the line nor a newline, and the
        // output starts on the prompt's row
        let under = |s: &LocalSession, first: &str, second: &str| {
            rows(s).windows(2).any(|w| w[0].trim_end().ends_with(first) && w[1].trim_end() == second)
        };
        // on a loaded machine bash 3.2 can print "child setpgid ...: Operation
        // not permitted" on the prompt's row first: its child and it both
        // set the job's group, and the loser of that race says so
        let first = |s: &LocalSession| under(s, "$ one", "two") || stacked(s, "one", "two");
        s.write(b"/usr/bin/printf 'one\\ntwo\\n'\r").unwrap();
        assert!(pump_until(&mut s, |s| first(s) && at_prompt(s)), "the first command:\n{}", screen(&s));
        s.write(b"/usr/bin/printf 'three\\nfour\\n'\r").unwrap();
        let later = |s: &LocalSession| under(s, "$ three", "four") || stacked(s, "three", "four");
        assert!(pump_until(&mut s, later), "later commands too:\n{}", screen(&s));
    }
}
