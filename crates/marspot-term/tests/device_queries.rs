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

/// What a probe must get back.
enum Expect {
    /// The exact bytes.  Used where the reply is a fixed contract —
    /// change it and you have changed what marspot claims to be.
    Exact(&'static [u8]),
    /// A prefix, where the tail is a live value (a colour, a position).
    Prefix(&'static [u8]),
    /// Nothing.  Not an absence of thought: each of these is a
    /// capability marspot does not have, and the spec's answer to
    /// "can you do X" when the answer is no is to say nothing, so the
    /// program stays on the path it would have taken anyway.  If one
    /// of these ever gets implemented, this row is where it gets
    /// changed — the row is the record of the decision.
    Silent,
}

/// Every probe a program is likely to open with, and what it gets.
///
/// The point of the table is the silent half.  A reply arm that stops
/// being reachable is invisible: the code still reads correctly, the
/// comment still describes the reply, and nothing fails.  It happened
/// twice — XTQVERSION (found 2026-09-07) and DA2 (found 2026-09-20)
/// both sat below an early return that made them dead, each with a
/// comment above it describing the answer it was not sending.
const PROBES: &[(&str, &[u8], Expect)] = &[
    ("DA1", b"\x1b[c", Expect::Exact(b"\x1b[?62;1;6;22c")),
    ("DA1 with explicit 0", b"\x1b[0c", Expect::Exact(b"\x1b[?62;1;6;22c")),
    ("DA2", b"\x1b[>c", Expect::Exact(b"\x1b[>41;330;0c")),
    ("DECID", b"\x1bZ", Expect::Exact(b"\x1b[?62;1;6;22c")),
    ("DSR status", b"\x1b[5n", Expect::Exact(b"\x1b[0n")),
    ("CPR", b"\x1b[6n", Expect::Exact(b"\x1b[1;1R")),
    ("DECXCPR", b"\x1b[?6n", Expect::Exact(b"\x1b[?1;1;1R")),
    ("XTVERSION", b"\x1b[>0q", Expect::Exact(b"\x1bP>|marspot\x1b\\")),
    ("DECRQM, a mode we have", b"\x1b[?2026$p", Expect::Exact(b"\x1b[?2026;2$y")),
    ("DECRQM, a mode we don't", b"\x1b[?9$p", Expect::Exact(b"\x1b[?9;0$y")),
    ("OSC 10 foreground", b"\x1b]10;?\x1b\\", Expect::Prefix(b"\x1b]10;rgb:")),
    ("OSC 11 background", b"\x1b]11;?\x1b\\", Expect::Prefix(b"\x1b]11;rgb:")),
    ("OSC 12 cursor colour", b"\x1b]12;?\x1b\\", Expect::Prefix(b"\x1b]12;rgb:")),
    ("OSC 4 palette entry", b"\x1b]4;1;?\x1b\\", Expect::Prefix(b"\x1b]4;1;rgb:")),
    ("XTWINOPS text size", b"\x1b[18t", Expect::Exact(b"\x1b[8;4;40t")),
    // The kitty keyboard reply is the flags actually in force, which
    // with nothing pushed is none of them.
    ("kitty keyboard query", b"\x1b[?u", Expect::Exact(b"\x1b[?0u")),
    // The silences below are refusals, each with a reason:
    //
    // DA3 — xterm answers with a "terminal unit id", a number this
    //   terminal does not have and would have to invent.
    // XTWINOPS 14/16 — the same area and one cell in PIXELS.  Those
    //   numbers live in the renderer, two processes from the
    //   emulator, and the only reason to ask is to place an image,
    //   which marspot does not display.
    // XTWINOPS 1-13, 15, 17, 19-24 — raise, move, resize, iconify,
    //   read the title back.  A program does not get to move this
    //   window or read text out of it; xterm disables most of these
    //   by default for the same reason.
    // XTSMGRAPHICS, kitty graphics — image protocols.  marspot draws
    //   text.  The protocols are built so that silence means no.
    // XTGETTCAP, DECRQSS — a capability database and a "what is the
    //   current SGR" readback.  Both have an answer already in reach
    //   of the program: terminfo for the first, its own bookkeeping
    //   for the second, which is why nothing asks.  Measured with a
    //   pty probe: claude, codex and vim send neither.
    // OSC 52 read — handing a program the user's clipboard because
    //   it asked.  That one is a refusal on purpose and stays one.
    ("DA3", b"\x1b[=c", Expect::Silent),
    ("XTWINOPS pixel size", b"\x1b[14t", Expect::Silent),
    ("XTWINOPS cell size", b"\x1b[16t", Expect::Silent),
    ("XTWINOPS move window", b"\x1b[3;0;0t", Expect::Silent),
    ("XTWINOPS report title", b"\x1b[21t", Expect::Silent),
    ("XTSMGRAPHICS", b"\x1b[?1;1;0S", Expect::Silent),
    ("XTGETTCAP", b"\x1bP+q544e\x1b\\", Expect::Silent),
    ("DECRQSS", b"\x1bP$qm\x1b\\", Expect::Silent),
    ("kitty graphics", KITTY_PROBE, Expect::Silent),
    ("OSC 52 clipboard read", b"\x1b]52;c;?\x1b\\", Expect::Silent),
];

#[test]
fn probe_replies() {
    for (name, probe, expect) in PROBES {
        both_ways(probe, |t, how| {
            let got = t.take_response();
            match expect {
                Expect::Exact(want) => assert_eq!(
                    got, *want,
                    "{name}, {how}: got {:?} want {:?}",
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(want)
                ),
                Expect::Prefix(want) => assert!(
                    got.starts_with(want),
                    "{name}, {how}: got {:?}",
                    String::from_utf8_lossy(&got)
                ),
                Expect::Silent => assert!(
                    got.is_empty(),
                    "{name}, {how}: answered {:?}",
                    String::from_utf8_lossy(&got)
                ),
            }
            // Whatever the answer, a probe is not text.
            assert_eq!(row0(t, 40), " ".repeat(40), "{name}, {how}");
        });
    }
}

#[test]
fn decrqm_reports_the_mode_it_was_just_given() {
    // The reply has to track the terminal, not a table of constants:
    // a program turns synchronized output on and then confirms it.
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[?2026$p");
    assert_eq!(t.take_response(), b"\x1b[?2026;2$y", "reset before it is set");
    t.feed(b"\x1b[?2026h\x1b[?2026$p");
    assert_eq!(t.take_response(), b"\x1b[?2026;1$y", "set after DECSET");
    t.feed(b"\x1b[?2026l\x1b[?2026$p");
    assert_eq!(t.take_response(), b"\x1b[?2026;2$y", "reset after DECRST");
}

#[test]
fn decrqm_is_honest_about_the_two_modes_with_no_switch() {
    // 3 = permanently set, 4 = permanently reset.  Wrap cannot be
    // turned off here and origin mode cannot be turned on, and saying
    // so is more useful to a program than claiming not to know.
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[?7$p\x1b[?6$p");
    assert_eq!(t.take_response(), b"\x1b[?7;3$y\x1b[?6;4$y");
}
