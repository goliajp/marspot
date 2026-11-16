//! Pointer events for the pane's program, checked here before a byte of
//! them reaches the tty.
//!
//! The core decides to send a mouse report from the modes in the last
//! frame it read.  That copy lags: by a frame at best, and by the whole
//! hold while the grid is held, because held output is not parsed.  A
//! report sent on a stale copy after the program has gone is typed into
//! the shell: on 2026-11-15 a profile cycle held a pane while claude
//! exited, the pointer moved over it, and zsh ran `35;3;1M35;2;2M…` as
//! commands -- `3` and `2` being `cd -3` and `cd -2` there.
//!
//! So the session encodes the event itself, from its own modes, at the
//! moment it writes, and writes nothing when:
//!
//! - the grid is held: its modes are as stale as the core's;
//! - the program did not ask for this event (mode, or alt screen for
//!   the wheel as arrows);
//! - no program other than the shell has the tty -- see
//!   [`program_has_the_tty`].

use std::os::fd::RawFd;

use marspot_term::shell_proto::{PointerInput, PointerKind};
use marspot_term::terminal::{MouseTrackingMode, Terminal};
use marspot_term::{lx_event, lx_warn};

use crate::SessionImpl;
use crate::frames::Frames;

/// Most reports or arrows one event writes.  The core caps a wheel
/// event at a screenful; this bounds the stack buffer.
const MAX_COUNT: usize = 256;
/// Longest single report: `ESC [ < 65 ; 4294967295 ; 4294967295 M`.
const MAX_REPORT: usize = 3 + 3 + 1 + 10 + 1 + 10 + 1;

/// Why an event was not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Held,
    NotAsked,
    NoProgram,
}

/// The modes an event is checked and encoded against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Modes {
    pub tracking: MouseTrackingMode,
    pub sgr: bool,
    pub alt_screen: bool,
    pub app_cursor: bool,
}

impl Modes {
    pub(crate) fn of(t: &Terminal) -> Self {
        Self {
            tracking: t.mouse_tracking_mode(),
            sgr: t.mouse_sgr_encoding(),
            alt_screen: t.in_alt_screen(),
            app_cursor: t.cursor_key_application_mode(),
        }
    }
}

/// Did the program ask for this event?
pub(crate) fn asked(ev: &PointerInput, m: &Modes) -> bool {
    use MouseTrackingMode::*;
    match ev.kind {
        PointerKind::Press | PointerKind::Release | PointerKind::Wheel => m.tracking != Off,
        // 3 is "no button": only `?1003` wants that travel
        PointerKind::Motion if ev.button == 3 => m.tracking == AnyEvent,
        PointerKind::Motion => matches!(m.tracking, ButtonEvent | AnyEvent),
        // the core's own routing: arrows only where no reports are wanted
        PointerKind::Arrows => m.alt_screen && m.tracking == Off,
        PointerKind::ProgramKeys => true,
    }
}

/// A fixed buffer, so the pointer path allocates nothing.
pub(crate) struct Out {
    buf: [u8; MAX_COUNT * MAX_REPORT],
    len: usize,
}

impl Out {
    pub(crate) fn new() -> Self {
        Self {
            buf: [0; MAX_COUNT * MAX_REPORT],
            len: 0,
        }
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    fn push(&mut self, b: &[u8]) {
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
    }
    fn push_u32(&mut self, mut v: u32) {
        let mut digits = [0u8; 10];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        digits[..n].reverse();
        self.push(&digits[..n]);
    }
}

/// One report, in the encoding the program asked for.  X10 packs each
/// field into a byte at `+32` and stops at 223; that is the encoding's
/// own ceiling.
fn report(out: &mut Out, sgr: bool, button: u8, pressed: bool, col: u32, row: u32) {
    if sgr {
        out.push(b"\x1b[<");
        out.push_u32(button as u32);
        out.push(b";");
        out.push_u32(col);
        out.push(b";");
        out.push_u32(row);
        out.push(if pressed { b"M" } else { b"m" });
    } else {
        // X10 has no release code per button: every release is 3
        let b = if pressed { button } else { 3 };
        out.push(&[
            0x1b,
            b'[',
            b'M',
            b.wrapping_add(32),
            col.min(223) as u8 + 32,
            row.min(223) as u8 + 32,
        ]);
    }
}

/// The bytes for `ev` under `m` into `out`; `false` when the program
/// did not ask for it.  `ProgramKeys` carries its own bytes and puts
/// nothing in `out`.
pub(crate) fn encode(ev: &PointerInput, m: &Modes, out: &mut Out) -> bool {
    if !asked(ev, m) {
        return false;
    }
    let n = (ev.count as usize).clamp(1, MAX_COUNT);
    match ev.kind {
        PointerKind::Press => report(out, m.sgr, ev.button, true, ev.col, ev.row),
        PointerKind::Release => report(out, m.sgr, ev.button, false, ev.col, ev.row),
        // the motion bit is what tells travel from a press
        PointerKind::Motion => report(out, m.sgr, ev.button.wrapping_add(32), true, ev.col, ev.row),
        // a tick is a press with no release
        PointerKind::Wheel => {
            (0..n).for_each(|_| report(out, m.sgr, ev.button, true, ev.col, ev.row))
        }
        PointerKind::Arrows => {
            let seq: &[u8] = match (m.app_cursor, ev.button == 64) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1bOB",
                (false, true) => b"\x1b[A",
                (false, false) => b"\x1b[B",
            };
            (0..n).for_each(|_| out.push(seq));
        }
        PointerKind::ProgramKeys => {}
    }
    true
}

/// Shells whose prompt reads typed input as commands.  Matched on the
/// image's file name, so a shell started through a wrapper script, or
/// one that `exec`s another shell, is still recognised.
const SHELLS: &[&str] = &[
    "zsh", "bash", "sh", "dash", "ksh", "mksh", "tcsh", "csh", "fish", "nu", "elvish", "xonsh",
];

/// Is a program other than the shell reading the tty, so that a pointer
/// event has someone who asked for it?
///
/// - In canonical mode, no.  A program that asked for mouse reports or
///   arrow keys reads in raw or cbreak mode; a tty in canonical mode is
///   either a shell between its line editor's turns or a program that
///   has put the terminal back on its way out.  This is the gap the
///   2026-11-15 reports went through: claude had restored the terminal
///   and the tty echoed them, so zsh read them as typing.
/// - The foreground group is the shell's, and the process there still
///   runs the shell: no.  A shell that `exec`ed a program keeps its
///   pid, so the image decides -- `shell_image` (the shell spawned,
///   resolved) or any image named in [`SHELLS`].
/// - The foreground group is someone else's: yes while that group has a
///   live process.  A group whose last process has gone reads nothing.
pub(crate) fn program_has_the_tty(
    master: RawFd,
    shell_pid: libc::pid_t,
    shell_image: &[u8],
) -> bool {
    // SAFETY: termios is plain data; tcgetattr on the master this
    // session owns fills it or fails.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(master, &mut t) } != 0 || t.c_lflag & libc::ICANON != 0 {
        return false;
    }
    // SAFETY: tcgetpgrp on the pty master this session owns.
    let front = unsafe { libc::tcgetpgrp(master) };
    if front <= 0 {
        return false;
    }
    if front != shell_pid {
        // SAFETY: signal 0 only probes; it delivers nothing.
        let r = unsafe { libc::kill(-front, 0) };
        return r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }
    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as long as the size passed.
    let n = unsafe { libc::proc_pidpath(shell_pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if n <= 0 {
        return false;
    }
    let image = &path[..n as usize];
    if image == shell_image {
        return false;
    }
    let name = image.rsplit(|&b| b == b'/').next().unwrap_or(image);
    !SHELLS.iter().any(|s| s.as_bytes() == name)
}

/// The shell this session spawns, resolved the way the kernel reports
/// an image: `$SHELL`, or `/bin/zsh` -- the same choice `spawn` makes.
pub(crate) fn shell_image() -> &'static [u8] {
    static IMAGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    IMAGE.get_or_init(|| {
        use std::os::unix::ffi::OsStrExt;
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
        let real = std::fs::canonicalize(&shell).unwrap_or_else(|_| shell.into());
        real.as_os_str().as_bytes().to_vec()
    })
}

/// Pointer events through the checks above, logging each change of
/// verdict once: a moving pointer is many events.
pub(crate) struct Gate {
    out: Box<Out>,
    last: Option<Refusal>,
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            out: Box::new(Out::new()),
            last: None,
        }
    }

    /// The bytes to write for `ev`, or why there are none.  `tty` is
    /// asked only once the modes say the program wants the event.
    pub(crate) fn check<'a>(
        &'a mut self,
        ev: &'a PointerInput,
        holding: bool,
        modes: &Modes,
        tty: impl FnOnce() -> bool,
    ) -> Result<&'a [u8], Refusal> {
        self.out.len = 0;
        if holding {
            return Err(Refusal::Held);
        }
        if !encode(ev, modes, &mut self.out) {
            return Err(Refusal::NotAsked);
        }
        if !tty() {
            return Err(Refusal::NoProgram);
        }
        Ok(if ev.kind == PointerKind::ProgramKeys {
            &ev.keys
        } else {
            self.out.bytes()
        })
    }

    /// Write `ev` to the pty if it passes, and log a change of verdict.
    pub(crate) fn deliver(
        &mut self,
        session: &mut SessionImpl,
        frames: &mut Frames,
        ev: &PointerInput,
    ) {
        let modes = Modes::of(session.terminal());
        let holding = session.is_holding();
        let verdict = match self.check(ev, holding, &modes, || session.program_has_the_tty()) {
            Ok(bytes) => {
                frames.typed(session);
                write_input(session, bytes);
                None
            }
            Err(why) => Some(why),
        };
        let id = session.id();
        self.note(id, ev.kind, verdict);
    }

    /// Log a verdict when it differs from the last one.
    pub(crate) fn note(&mut self, session_id: u64, kind: PointerKind, verdict: Option<Refusal>) {
        if verdict == self.last {
            return;
        }
        self.last = verdict;
        match verdict {
            Some(why) => lx_event!(
                "L3_POINTER_DROPPED",
                "pointer events are not reaching the pty",
                session_id = session_id,
                kind = format!("{kind:?}"),
                why = format!("{why:?}")
            ),
            None => lx_event!(
                "L3_POINTER_WRITTEN",
                "pointer events reach the pty again",
                session_id = session_id,
                kind = format!("{kind:?}")
            ),
        }
    }
}

/// Write input to the pty.  A dead pty means the shell exited and the
/// session is torn down next, so that stays quiet; a full queue is
/// worth a line: the foreground process has stopped reading stdin and
/// what was typed is being dropped.
pub(crate) fn write_input(session: &mut SessionImpl, bytes: &[u8]) {
    if let Err(e) = session.write(bytes)
        && e.kind() == std::io::ErrorKind::WouldBlock
    {
        lx_warn!(
            "l3.pty.write_dropped",
            &format!("{e}"),
            bytes = bytes.len(),
            backlog = session.pty_write_backlog()
        );
    }
}

impl SessionImpl {
    /// Whether a program other than the shell reads the tty now.
    pub(crate) fn program_has_the_tty(&self) -> bool {
        match self {
            Self::Local(s) => program_has_the_tty(s.raw_master(), s.child_pid(), shell_image()),
        }
    }
}

#[cfg(test)]
#[path = "pointer_tests.rs"]
mod tests;
