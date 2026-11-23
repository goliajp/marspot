//! `PaneType`: input written in order, the return after it, and each
//! way it can be refused.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::TypedResult;
use marspot_term::test_env::sandbox_state_dir;

use crate::pane_link::{Link, wait, with_script, with_zsh};

/// What the program read, one line per read: seconds, then hex bytes.
const LOGGER: &str = r#"system("stty raw -echo"); $| = 1; print "\e[?2004hREADY";
open(my $f, ">", $ARGV[0]) or die; select((select($f), $| = 1)[0]);
while (sysread(STDIN, my $b, 65536)) { printf $f "%.6f %s\n", time, unpack("H*", $b); }"#;

fn reads(log: &std::path::Path) -> Vec<(f64, Vec<u8>)> {
    let body = std::fs::read_to_string(log).unwrap_or_default();
    body.lines()
        .filter_map(|l| {
            let (t, hex) = l.split_once(' ')?;
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
            Some((t.parse().ok()?, bytes))
        })
        .collect()
}

#[test]
fn typing_pastes_then_returns_after_the_gap() {
    let mut sandbox = sandbox_state_dir();
    let log = sandbox.dir.join("log");
    let pl = sandbox.dir.join("log.pl");
    std::fs::write(&pl, format!("use Time::HiRes qw(time);\n{LOGGER}\n")).unwrap();
    let s = with_script(&mut sandbox, &format!("exec /usr/bin/perl {} {}", pl.display(), log.display()));
    let mut l = Link::new(&s);
    let req = l.type_in(b"echo hi", true, true, false, None);
    let t = l.typed(req, 5.0);
    assert_eq!(t.result, TypedResult::Written);
    assert!(!t.echo_off, "a raw program draws its own echo");
    let want = b"\x1b[200~echo hi\x1b[201~\r";
    assert_eq!(t.written as usize, want.len());
    wait(5.0, "the program to read the return", || reads(&log).iter().any(|(_, b)| b.ends_with(b"\r")));
    let got = reads(&log);
    let all: Vec<u8> = got.iter().flat_map(|(_, b)| b.clone()).collect();
    assert_eq!(all, want, "the paste, then the return, nothing else");
    let pasted = got.iter().find(|(_, b)| b.ends_with(b"\x1b[201~")).expect("a read that ends the paste").0;
    let returned = got.last().unwrap().0;
    // the program prints nothing, so the return waits the whole 50 ms
    assert!(returned - pasted >= 0.045, "the return came {:.1} ms after the paste", (returned - pasted) * 1e3);
}

/// Output from before the input was written is not its echo: the
/// program prints while the input waits unwritten, then goes quiet and
/// reads it, and the return still waits its 50 ms.
#[test]
fn typing_output_while_the_input_waits_is_not_its_echo() {
    let mut sandbox = sandbox_state_dir();
    let log = sandbox.dir.join("log");
    let pl = sandbox.dir.join("log.pl");
    let script = format!(
        r#"use Time::HiRes qw(time sleep);
system("stty raw -echo"); $| = 1; print "READY\r\n";
my $end = time + 0.8; while (time < $end) {{ print "busy\r\n"; sleep 0.01; }}
sleep 0.2;
open(my $f, ">", "{}") or die; select((select($f), $| = 1)[0]);
while (sysread(STDIN, my $b, 65536)) {{ printf $f "%.6f %s\n", time, unpack("H*", $b); }}
"#,
        log.display()
    );
    std::fs::write(&pl, script).unwrap();
    let s = with_script(&mut sandbox, &format!("exec /usr/bin/perl {}", pl.display()));
    let mut l = Link::new(&s);
    // more than a raw tty takes before its program reads
    let input = [b'a'; 4096];
    let req = l.type_in(&input, false, true, false, None);
    assert_eq!(l.typed(req, 5.0).result, TypedResult::Written);
    wait(5.0, "the program to read the return", || reads(&log).iter().any(|(_, b)| b.ends_with(b"\r")));
    let got = reads(&log);
    let mut count = 0;
    let last_input = got
        .iter()
        .find(|(_, b)| {
            count += b.iter().filter(|&&c| c == b'a').count();
            count == input.len()
        })
        .expect("a read that ends the input")
        .0;
    let returned = got.iter().find(|(_, b)| b.ends_with(b"\r")).unwrap().0;
    assert!(returned - last_input >= 0.045, "the return came {:.1} ms after the input", (returned - last_input) * 1e3);
}

#[test]
fn typing_the_ninth_queued_is_refused() {
    let mut sandbox = sandbox_state_dir();
    let s = with_script(&mut sandbox, "stty raw -echo; printf READY; exec sleep 30");
    let mut l = Link::new(&s);
    // more than a raw tty takes: this one stays unwritten
    let first = l.type_in(&[b'a'; 4096], false, false, false, None);
    for _ in 0..7 {
        l.type_in(b"x", false, false, false, None);
    }
    let started = Instant::now();
    let ninth = l.type_in(b"x", false, false, false, None);
    let t = l.typed(ninth, 1.5);
    assert_eq!(t.result, TypedResult::QueueFull);
    assert_eq!(t.written, 0);
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(l.typed(first, 5.0).result, TypedResult::NotReading);
}

#[test]
fn typing_into_a_program_that_does_not_read_gives_up_after_two_seconds_without_the_return() {
    let mut sandbox = sandbox_state_dir();
    let got = sandbox.dir.join("got");
    let s = with_script(
        &mut sandbox,
        &format!("stty raw -echo; printf READY; sleep 3; exec cat > {}", got.display()),
    );
    let mut l = Link::new(&s);
    let started = Instant::now();
    let req = l.type_in(&[b'a'; 4096], false, true, false, None);
    let t = l.typed(req, 5.0);
    let took = started.elapsed();
    assert_eq!(t.result, TypedResult::NotReading);
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_millis(2900), "answered after {took:?}");
    assert!(t.written < 4096, "{} bytes reported written to a program that reads nothing", t.written);
    wait(10.0, "the program to read the input", || std::fs::metadata(&got).is_ok_and(|m| m.len() >= 4096));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read(&got).unwrap(), vec![b'a'; 4096], "the input and no return");
}

#[test]
fn typing_into_a_full_pty_queue_writes_nothing() {
    let mut sandbox = sandbox_state_dir();
    let s = with_script(&mut sandbox, "stty raw -echo; printf READY; exec sleep 30");
    let mut l = Link::new(&s);
    // 256 KiB, the queue's cap, of which a raw tty takes one chunk's start
    for _ in 0..8 {
        l.inject(&"z".repeat(32 * 1024));
    }
    let req = l.type_in(b"hello", false, true, false, None);
    let t = l.typed(req, 5.0);
    assert_eq!(t.result, TypedResult::PtyQueueFull);
    assert_eq!(t.written, 0);
}

#[test]
fn typing_says_when_the_terminal_reads_lines_without_echo() {
    for (stty, off) in [("-echo", true), ("echo", false)] {
        let mut sandbox = sandbox_state_dir();
        let s = with_script(&mut sandbox, &format!("stty {stty} icanon; printf READY; exec cat > /dev/null"));
        let mut l = Link::new(&s);
        let req = l.type_in(b"secret", false, true, false, None);
        let t = l.typed(req, 5.0);
        assert_eq!(t.result, TypedResult::Written);
        assert_eq!(t.echo_off, off, "stty {stty}");
    }
}

#[test]
fn typing_only_at_the_shell_is_refused_while_a_command_runs() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let req = l.type_in(b"", false, false, true, None);
    assert_eq!(l.typed(req, 5.0).result, TypedResult::Written, "at the prompt");
    l.inject("sleep 30\r");
    let mut last = None;
    wait(10.0, "the command to take the foreground", || {
        let req = l.type_in(b"x", false, false, true, None);
        let t = l.typed(req, 5.0);
        last = Some(t);
        t.result == TypedResult::NotAtShell
    });
    assert_eq!(last.unwrap().written, 0);
}

/// A program that keeps bracketed paste on across a return still gets
/// the paste typed after it in markers, once it has answered the
/// return.
#[test]
fn typing_a_paste_after_a_return_keeps_its_markers_when_the_mode_stays_on() {
    let mut sandbox = sandbox_state_dir();
    let log = sandbox.dir.join("log");
    let pl = sandbox.dir.join("log.pl");
    // like LOGGER, and it answers every read with a dot
    let script = r#"use Time::HiRes qw(time);
system("stty raw -echo"); $| = 1; print "\e[?2004hREADY";
open(my $f, ">", $ARGV[0]) or die; select((select($f), $| = 1)[0]);
while (sysread(STDIN, my $b, 65536)) { print "."; printf $f "%.6f %s\n", time, unpack("H*", $b); }"#;
    std::fs::write(&pl, script).unwrap();
    let s = with_script(&mut sandbox, &format!("exec /usr/bin/perl {} {}", pl.display(), log.display()));
    let mut l = Link::new(&s);
    let first = l.type_in(b"a", true, true, false, None);
    let second = l.type_in(b"b", true, false, false, None);
    assert_eq!(l.typed(first, 5.0).result, TypedResult::Written);
    assert_eq!(l.typed(second, 5.0).result, TypedResult::Written);
    wait(5.0, "the program to read the second paste", || reads(&log).iter().any(|(_, b)| b.ends_with(b"b\x1b[201~")));
    let got = reads(&log);
    let all: Vec<u8> = got.iter().flat_map(|(_, b)| b.clone()).collect();
    assert_eq!(all, b"\x1b[200~a\x1b[201~\r\x1b[200~b\x1b[201~", "both pastes in markers");
    let returned = got.iter().find(|(_, b)| b.ends_with(b"\r")).unwrap().0;
    let gap = got.last().unwrap().0 - returned;
    // the echo wait, and well short of the two seconds a silent program gets
    assert!((0.045..1.5).contains(&gap), "the second paste came {:.1} ms after the return", gap * 1e3);
}
