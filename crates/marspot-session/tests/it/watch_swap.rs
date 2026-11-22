//! What an image swap does to pane control: the position the next
//! image starts from, and every request and watch the old image held
//! answered before it goes.

use std::path::Path;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{DoneCode, MsgType, TypedResult, WatchKind, WatchReason};

use crate::pane_link::{Link, Reply, bin, body, wait};
use crate::self_execv::{Sandbox, count_in_log, id, stage};
use crate::support::{COLS, ROWS, Session};

/// A pane whose shell is `script` (a `/bin/sh` script body).
fn spawn(sandbox: &mut Sandbox, script: &str) -> Session {
    use std::os::unix::fs::PermissionsExt;
    let path = sandbox.path().join("pane.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.env.set("SHELL", &path);
    Session::spawn_with_id(bin(), COLS, ROWS, Some(id()))
}

/// Ask for the swap and reattach to the image that took over.
fn swap(s: &Session, state: &Path) -> Link {
    stage(state, true);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    let pid = s.pid() as i32;
    wait(30.0, "the new image", || {
        count_in_log(state, "L3_EXECV_RESUMED") >= 1 && unsafe { libc::kill(pid, 0) } == 0
    });
    let socket = marspot_term::session_registry::session_socket_path(id());
    Link::on(crate::support::reattach(&socket, 15).expect("reattach after the swap"))
}

#[test]
fn watch_position_after_a_swap_in_a_flood_is_the_bytelog_length() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let stop = state.join("stop");
    let s = spawn(&mut sandbox, &format!("while [ ! -e {} ]; do seq 1 3000; done\nexec sleep 600", stop.display()));
    let dir = state.join(format!("sessions/{}", id()));
    let logged = || marspot_term::bytelog::stream_len(&dir).unwrap_or(0);
    wait(20.0, "the flood to be well under way", || logged() > 1 << 19);
    let mut l = swap(&s, &state);
    std::fs::write(&stop, b"").unwrap();
    // the flood ends: the position stops moving
    let mut seen = (l.pos(), Instant::now());
    wait(30.0, "the output to stop", || {
        let p = l.pos();
        if p != seen.0 {
            seen = (p, Instant::now());
        }
        seen.1.elapsed() >= Duration::from_millis(500)
    });
    let pos = l.pos();
    wait(10.0, "the bytelog to have every byte", || logged() >= pos);
    assert_eq!(pos, logged(), "the position the new image counts from is the bytelog's");
}

#[test]
fn watch_swap_answers_input_in_flight_and_reports_a_refusing_writer_took_late() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    // the first two reports are refused, as by a full writer queue
    sandbox.env.set("MARSPOT_TEST_REFUSE_WATCH_REPORTS", "2");
    let s = spawn(&mut sandbox, "stty raw -echo; printf READY; exec sleep 600");
    crate::pane_link::screen_shows(&s, "READY");
    let mut l = Link::new(&s);
    l.armed(body(1, WatchKind::Change, u64::MAX - 1));
    // more than a raw tty takes from a program that reads nothing, then
    // a full queue behind it: each is given up after 2 s unwritten, so
    // however long the new image takes to start (under 16 s), some are
    // still held when the swap comes
    let mut reqs = vec![l.type_in(&[b'a'; 4096], false, true, false, None)];
    reqs.extend((0..7).map(|_| l.type_in(&[b'x'; 2048], false, false, false, None)));
    stage(&state, true);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    assert_eq!(l.fired(1, 30.0).reason, WatchReason::Restarting, "told, though the writer refused twice");
    let mut restarting = 0;
    for req in reqs {
        let answer = l.take(5.0, &format!("an answer to {req}"), |r| match r {
            Reply::Typed(t) if t.req_id == req => Some(Err(t.result)),
            Reply::Done(d) if d.req_id == req => Some(Ok((d.code, d.text.clone()))),
            _ => None,
        });
        match answer {
            Err(result) => {
                assert_eq!(result, TypedResult::NotReading, "request {req}");
                assert_eq!(restarting, 0, "request {req} was typed after one before it was dropped");
            }
            Ok(done) => {
                assert_eq!(done, (DoneCode::Other, "restarting".to_string()), "request {req}");
                restarting += 1;
            }
        }
    }
    assert!(restarting > 0, "every request was given up before the swap: none tested");
    std::thread::sleep(Duration::from_millis(300));
    assert!(l.try_take(0.2, |r| matches!(r, Reply::Typed(_) | Reply::Done(_)).then_some(())).is_none(), "one answer each");
}
