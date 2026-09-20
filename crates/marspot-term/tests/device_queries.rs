//! What a program's probes must leave on the screen: nothing.
//!
//! A terminal is asked what it can do before it is asked to draw
//! anything.  Those probes are strings — APC for kitty graphics, DCS
//! for terminfo capabilities, OSC for the background colour — and a
//! parser that does not consume a string prints it.  Claude Code's
//! kitty-graphics probe did exactly that: it landed on top of its own
//! trust prompt as `v=1, a=q, t=d, f=24; AAAA`, with the two choices
//! shoved out of place (reported 2026-09-20).
//!
//! The parser's own tests assert on the events it emits.  These assert
//! on the screen, which is the thing the user is looking at, and they
//! feed the bytes one at a time as well as whole — a probe arrives
//! split across reads as a matter of course, and a state machine that
//! only swallows a string when it comes in one piece leaks on a slow
//! pty.
use marspot_term::terminal::Terminal;

/// The exact probe Claude Code opens with: is there kitty graphics?
const KITTY_PROBE: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

fn row0(t: &Terminal, n: usize) -> String {
    (0..n as u16).map(|c| t.grid().cell(c, 0).ch).collect()
}

/// Feed `bytes` whole, then one byte at a time, and check both.
fn both_ways(bytes: &[u8], check: impl Fn(&mut Terminal, &str)) {
    let mut whole = Terminal::new(40, 4);
    whole.feed(bytes);
    check(&mut whole, "fed whole");

    let mut split = Terminal::new(40, 4);
    for b in bytes {
        split.feed(&[*b]);
    }
    check(&mut split, "fed one byte at a time");
}

#[test]
fn a_kitty_graphics_probe_leaves_the_screen_untouched() {
    both_ways(KITTY_PROBE, |t, how| {
        assert_eq!(row0(t, 40), " ".repeat(40), "{how}");
        assert_eq!(t.grid().cursor(), (0, 0), "{how}");
    });
}

#[test]
fn a_probe_before_a_prompt_does_not_displace_it() {
    // The shape of the report: the probe, then the program's own text.
    // The text has to start at column 0.
    let mut bytes = KITTY_PROBE.to_vec();
    bytes.extend_from_slice("Yes, I trust this folder".as_bytes());
    both_ways(&bytes, |t, how| {
        assert_eq!(row0(t, 24), "Yes, I trust this folder", "{how}");
    });
}

#[test]
fn dcs_sos_and_pm_payloads_stay_off_the_screen() {
    // Same state, three other introducers.  A DCS reply to XTGETTCAP
    // carries hex digits that read as perfectly ordinary text.
    for (name, bytes) in [
        ("DCS", b"\x1bP1+r5463=5A\x1b\\ok".as_slice()),
        ("SOS", b"\x1bXanything at all\x1b\\ok".as_slice()),
        ("PM", b"\x1b^private message\x1b\\ok".as_slice()),
    ] {
        both_ways(bytes, |t, how| {
            assert_eq!(row0(t, 2), "ok", "{name}, {how}");
        });
    }
}

#[test]
fn a_background_query_closed_with_st_is_answered() {
    // OSC 11 with `?` asks what the background is.  Closed with ST —
    // the form used at least as often as BEL — the payload used to be
    // dropped, and an unanswered query leaves a TUI guessing whether
    // it is on a dark or a light terminal.
    both_ways(b"\x1b]11;?\x1b\\", |t, how| {
        let reply = String::from_utf8(t.take_response()).expect("utf-8 reply");
        assert!(reply.starts_with("\x1b]11;rgb:"), "{how}: got {reply:?}");
        assert_eq!(row0(t, 40), " ".repeat(40), "{how}");
    });
}

#[test]
fn a_title_closed_with_st_is_kept() {
    both_ways(b"\x1b]0;hello\x1b\\", |t, how| {
        assert_eq!(t.osc_title(), "hello", "{how}");
        assert_eq!(row0(t, 40), " ".repeat(40), "{how}");
    });
}
