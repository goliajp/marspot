//! What a waiting text watch costs the parse.
//!
//! The session's real path is driven -- chunks taken in through
//! `LocalSession::pump`, the bytelog counter, the text prefilter and the
//! loop's `after_turn` -- without a PTY to read, so that the parse is
//! all there is to time.  Each round parses the same output once
//! without a watch and once with one, alternating, and the cheapest of
//! each is compared, in instructions: a host that is busy for all of
//! the rounds of one of them moves even the fastest time by several
//! percent, and the instructions hardly at all.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{PaneWatch, WatchBody, WatchKind};

use crate::SessionImpl;
use crate::local_session::LocalSession;
use crate::pane_ctl::{PaneCtl, Request};

const CHUNK: usize = 64 * 1024;
const ROUNDS: usize = 7;

/// Lines like a build's: words, numbers, colour.  No `@` in them.
fn output(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 128);
    let mut n = 0u64;
    while out.len() < bytes {
        let line = format!("   Compiling crate-{n:06} v0.{}.{} (\x1b[32mok\x1b[0m) in {}ms\r\n", n % 7, n % 13, n % 997);
        out.extend_from_slice(line.as_bytes());
        n += 1;
    }
    out
}

/// A full-screen program's frames on the alternate screen: each drawn
/// with the cursor hidden, rows addressed, colour, no line feed.  No
/// `@` in them, so a chunk is skipped but for its last escape.
fn frames(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 4096);
    out.extend_from_slice(b"\x1b[?1049h");
    let mut n = 0u64;
    while out.len() < bytes {
        out.extend_from_slice(b"\x1b[?25l");
        for row in 1..=40 {
            let line = format!("\x1b[{row};1H\x1b[38;5;{}m frame {n:06} row {row:02} status ok\x1b[0m\x1b[K", (n + row) % 256);
            out.extend_from_slice(line.as_bytes());
        }
        out.extend_from_slice(b"\x1b[?25h");
        n += 1;
    }
    out
}

/// How a text watch is checked, for comparing the two ways.
#[derive(Clone, Copy)]
enum Check {
    /// After the bytes said it can have matched.
    Stream,
    /// On the grid after every batch.
    Grid,
}

/// What a parse cost.
#[derive(Clone, Copy)]
struct Cost {
    took: Duration,
    /// Instructions this process retired meanwhile; 0 on a host that
    /// does not count them (a virtual machine).
    instructions: u64,
}

impl Cost {
    const MOST: Self = Self { took: Duration::MAX, instructions: u64::MAX };

    fn min(self, o: Self) -> Self {
        Self { took: self.took.min(o.took), instructions: self.instructions.min(o.instructions) }
    }
}

fn instructions() -> u64 {
    // SAFETY: an all-zero rusage_info_v4 is valid, and RUSAGE_INFO_V4
    // tells the kernel to fill exactly that struct
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast());
        assert_eq!(rc, 0, "proc_pid_rusage");
        info.ri_instructions
    }
}

/// What it costs to parse `out` with a text watch for `text`, if any.
fn parse(out: &[u8], text: Option<(&str, Check)>) -> Cost {
    let (local, tx) = LocalSession::parsing_only(120, 40);
    let mut session = SessionImpl::Local(local);
    let (wake, _woken) = std::sync::mpsc::channel();
    let mut ctl = PaneCtl::new(&session, Default::default(), wake);
    if let Some((text, check)) = text {
        let since = crate::pane_ctl::local(&session).output_pos();
        let watch = WatchBody { watch_id: 1, kind: WatchKind::Text, since_pos: since, quiet_ms: 0, text: text.into() };
        ctl.request(Request::Watch(PaneWatch { req_id: 1, sid: 1, watch }), &mut session, None);
        if let Check::Grid = check {
            ctl.check_every_batch();
        }
    }
    let chunks: Vec<Vec<u8>> = out.chunks(CHUNK).map(<[u8]>::to_vec).collect();
    let before = instructions();
    let started = Instant::now();
    for chunk in chunks {
        tx.send(chunk.into()).expect("queue a chunk");
        session.pump();
        ctl.after_turn(&mut session, None, false, None);
    }
    let took = started.elapsed();
    let cost = Cost { took, instructions: instructions() - before };
    assert_eq!(crate::pane_ctl::local(&session).output_pos(), out.len() as u64, "every byte counted once");
    cost
}

/// Least of `ROUNDS` without a watch and with one for `text`.
fn least(out: &[u8], text: &str, check: Check) -> (Cost, Cost) {
    let (mut bare, mut watched) = (Cost::MOST, Cost::MOST);
    // which goes first alternates, so neither is always the one after a
    // session was torn down
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            bare = bare.min(parse(out, None));
            watched = watched.min(parse(out, Some((text, check))));
        } else {
            watched = watched.min(parse(out, Some((text, check))));
            bare = bare.min(parse(out, None));
        }
    }
    (bare, watched)
}

fn mib_s(bytes: usize, d: Duration) -> f64 {
    bytes as f64 / d.as_secs_f64() / (1024.0 * 1024.0)
}

fn line(what: &str, bytes: usize, (bare, watched): (Cost, Cost)) -> f64 {
    let ratio = bare.instructions as f64 / watched.instructions as f64;
    let timed = bare.took.as_secs_f64() / watched.took.as_secs_f64();
    eprintln!(
        "watch perf, {what}: no watch {:.1} MiB/s, watched {:.1} MiB/s, ratio {timed:.3}; \
         instructions a byte {:.1} and {:.1}, ratio {ratio:.4}",
        mib_s(bytes, bare.took),
        mib_s(bytes, watched.took),
        bare.instructions as f64 / bytes as f64,
        watched.instructions as f64 / bytes as f64,
    );
    ratio
}

/// A text watch that never matches, whose last byte the output never
/// has, costs the parse no more than a twentieth of what it keeps: 95%
/// of the instructions with it are the parse's own.  The worst case,
/// a last byte on every line, is printed beside it, read off the bytes
/// as the session does and, for comparison, checked on the grid after
/// every batch; it keeps at least half.
#[test]
fn watch_pending_text_keeps_the_parse_within_five_percent() {
    let out = output(if cfg!(debug_assertions) { 4 } else { 32 } * 1024 * 1024);
    let worst = &out[..out.len() / 4];
    eprintln!(
        "watch perf: {} build, {} MiB and {} MiB, best of {ROUNDS}",
        if cfg!(debug_assertions) { "debug" } else { "release" },
        out.len() >> 20,
        worst.len() >> 20
    );
    let ratio = line("text never printed", out.len(), least(&out, "@never printed@", Check::Stream));
    let stream = line("last byte on every line", worst.len(), least(worst, "never printed ms", Check::Stream));
    line("last byte on every line, grid every batch", worst.len(), least(worst, "never printed ms", Check::Grid));
    if instructions() == 0 {
        eprintln!("skipping the assertion on a waiting text watch's cost: this host does not count instructions");
        return;
    }
    assert!(ratio >= 0.95, "with a waiting text watch {:.1}% of the instructions are the parse's", ratio * 100.0);
    assert!(stream >= 0.5, "in the worst case {:.1}% of the instructions are the parse's", stream * 100.0);
}

/// The same for a full-screen program's frames, whose chunks are
/// skipped: what is looked for in them is where the screen switches.
#[test]
fn watch_pending_text_on_a_full_screen_program_keeps_the_parse_within_five_percent() {
    let out = frames(if cfg!(debug_assertions) { 4 } else { 32 } * 1024 * 1024);
    eprintln!("watch perf: frames, {} MiB, best of {ROUNDS}", out.len() >> 20);
    let ratio = line("frames, text never printed", out.len(), least(&out, "@never printed@", Check::Stream));
    if instructions() == 0 {
        eprintln!("skipping the assertion on a waiting text watch's cost: this host does not count instructions");
        return;
    }
    assert!(ratio >= 0.95, "with a waiting text watch {:.1}% of the instructions are the parse's", ratio * 100.0);
}
