//! What a waiting text watch costs the parse.
//!
//! The session's real path is driven -- chunks taken in through
//! `LocalSession::pump`, the bytelog counter, the text prefilter and the
//! loop's `after_turn` -- without a PTY to read, so that the parse is
//! all there is to time.  Each round parses the same output once
//! without a watch and once with one, alternating, and the fastest of
//! each is compared: a host that is busy for a moment slows one round,
//! not the fastest.

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

/// How a text watch is checked, for comparing the two ways.
#[derive(Clone, Copy)]
enum Check {
    /// After the bytes said it can have matched.
    Stream,
    /// On the grid after every batch.
    Grid,
}

/// Seconds to parse `out` with a text watch for `text`, if any.
fn parse(out: &[u8], text: Option<(&str, Check)>) -> Duration {
    let (local, tx) = LocalSession::parsing_only(120, 40);
    let mut session = SessionImpl::Local(local);
    let (wake, _woken) = std::sync::mpsc::channel();
    let mut ctl = PaneCtl::new(&session, None, wake);
    if let Some((text, check)) = text {
        let since = crate::pane_ctl::local(&session).output_pos();
        let watch = WatchBody { watch_id: 1, kind: WatchKind::Text, since_pos: since, quiet_ms: 0, text: text.into() };
        ctl.request(Request::Watch(PaneWatch { req_id: 1, sid: 1, watch }), &mut session, None);
        if let Check::Grid = check {
            ctl.check_every_batch();
        }
    }
    let chunks: Vec<Vec<u8>> = out.chunks(CHUNK).map(<[u8]>::to_vec).collect();
    let started = Instant::now();
    for chunk in chunks {
        tx.send(chunk.into()).expect("queue a chunk");
        session.pump();
        ctl.after_turn(&mut session, false, None);
    }
    let took = started.elapsed();
    assert_eq!(crate::pane_ctl::local(&session).output_pos(), out.len() as u64, "every byte counted once");
    took
}

/// Fastest of `ROUNDS` without a watch and with one for `text`.
fn fastest(out: &[u8], text: &str, check: Check) -> (Duration, Duration) {
    let (mut bare, mut watched) = (Duration::MAX, Duration::MAX);
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

fn line(what: &str, bytes: usize, (bare, watched): (Duration, Duration)) -> f64 {
    let ratio = bare.as_secs_f64() / watched.as_secs_f64();
    eprintln!(
        "watch perf, {what}: no watch {:.1} MiB/s, watched {:.1} MiB/s, ratio {ratio:.3}",
        mib_s(bytes, bare),
        mib_s(bytes, watched)
    );
    ratio
}

/// A text watch that never matches, whose last byte the output never
/// has, keeps at least 95% of the parse's throughput.  The worst case,
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
    let ratio = line("text never printed", out.len(), fastest(&out, "@never printed@", Check::Stream));
    let stream = line("last byte on every line", worst.len(), fastest(worst, "never printed ms", Check::Stream));
    line("last byte on every line, grid every batch", worst.len(), fastest(worst, "never printed ms", Check::Grid));
    if !marspot_term::host_load::quiet_enough_to_time("a waiting text watch's cost to the parse") {
        return;
    }
    assert!(ratio >= 0.95, "a waiting text watch kept {:.1}% of the parse's throughput", ratio * 100.0);
    assert!(stream >= 0.5, "the worst case kept {:.1}% of the parse's throughput", stream * 100.0);
}
