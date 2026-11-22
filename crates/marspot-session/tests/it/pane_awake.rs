//! A session says when its pane prints and when another process group
//! takes its terminal, and says it on the control socket among its
//! other frames, so a reader that knows none of it reads past it.

use crate::support;

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{Frame, MsgType, Next, PaneAwake, decode_pane_awake};

/// Every frame read off the control socket within `within`; a frame of
/// a type the reader does not know counts too.  Stops early once
/// `done` holds for a `PaneAwake`.
fn read_for(s: &support::Session, within: Duration, done: impl Fn(&PaneAwake) -> bool) -> (Vec<PaneAwake>, usize) {
    s.control.set_read_timeout(Some(Duration::from_millis(100))).ok();
    let deadline = Instant::now() + within;
    let (mut awakes, mut others) = (Vec::new(), 0);
    let mut r = &s.control;
    while Instant::now() < deadline {
        match Frame::read_next(&mut r) {
            Ok(Some(Next::Frame(f))) if f.msg_type == MsgType::PaneAwake => {
                let a = decode_pane_awake(&f.payload).expect("a PaneAwake that decodes");
                let stop = done(&a);
                awakes.push(a);
                if stop {
                    break;
                }
            }
            Ok(Some(_)) => others += 1,
            Ok(None) => panic!("the session closed its control socket"),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("reading the control socket: {e}"),
        }
    }
    (awakes, others)
}

#[test]
fn output_and_a_change_of_foreground_are_reported_and_output_is_coalesced() {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let home = sandbox.dir.path().to_path_buf();
    sandbox.env.set("SHELL", "/bin/bash").set("HOME", &home);
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let s = support::Session::spawn(bin, support::COLS, support::ROWS);

    // where things stand, at once, and the shell at its prompt
    let (got, _) = read_for(&s, Duration::from_secs(10), |a| a.foreground && a.shell_in_front);
    let first = got.first().expect("a report as the core connected");
    assert!(first.foreground, "the first report gives the foreground");
    let shell = got.last().filter(|a| a.shell_in_front).expect("the shell in front").fg_pgid;
    assert!(shell > 0);
    std::thread::sleep(Duration::from_millis(500));
    let _ = read_for(&s, Duration::from_millis(200), |_| false);

    // a command that prints nothing still takes the terminal
    s.send_line("sleep 2");
    let (got, _) = read_for(&s, Duration::from_secs(5), |a| a.foreground && !a.shell_in_front);
    let job = got.last().filter(|a| a.foreground && !a.shell_in_front).expect("the job taking the terminal was reported");
    assert!(job.fg_pgid > 0 && job.fg_pgid != shell, "the job's group, not the shell's: {job:?}");
    let (got, _) = read_for(&s, Duration::from_secs(10), |a| a.foreground && a.shell_in_front);
    let back = got.last().filter(|a| a.foreground && a.shell_in_front).expect("the shell taking it back was reported");
    assert_eq!(back.fg_pgid, shell);
    std::thread::sleep(Duration::from_millis(300));
    let _ = read_for(&s, Duration::from_millis(200), |_| false);

    // forty lines over two seconds: reported about once a second, and
    // the frames after each report are still read
    s.send_line("for i in $(seq 40); do echo line$i; sleep 0.05; done");
    let (got, others) = read_for(&s, Duration::from_secs(4), |_| false);
    let outputs = got.iter().filter(|a| a.output).count();
    assert!((1..=5).contains(&outputs), "{outputs} output reports for two seconds of printing: {got:?}");
    assert!(others > 0, "no other frame came after the reports");
}
