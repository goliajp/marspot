//! Typing into and watching a real L3 the way the core forwards it:
//! `PaneType` and `PaneWatch` on the control socket, and the answers
//! read back off it.

use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{
    DoneCode, Frame, MsgType, PaneDone, PaneType, PaneTyped, PaneWatch, PaneWatchFired, TypedResult, WatchBody,
    WatchKind, decode_pane_done, decode_pane_typed, decode_pane_watch_fired, encode_pane_type, encode_pane_watch,
};

use crate::support::{COLS, ROWS, Session};

#[derive(Debug)]
pub(crate) enum Reply {
    Typed(PaneTyped),
    Done(PaneDone),
    Fired(PaneWatchFired),
}

/// The answers on one control stream, in the order they came.
pub(crate) struct Link {
    pub(crate) stream: UnixStream,
    got: VecDeque<Reply>,
    next_req: u32,
}

impl Link {
    pub(crate) fn new(s: &Session) -> Self {
        Self::on(s.control.try_clone().expect("clone the control socket"))
    }

    pub(crate) fn on(stream: UnixStream) -> Self {
        stream.set_read_timeout(Some(Duration::from_millis(20))).ok();
        Self { stream, got: VecDeque::new(), next_req: 1 }
    }

    fn send(&mut self, msg_type: MsgType, payload: Vec<u8>) {
        let mut w = &self.stream;
        Frame::new(msg_type, payload).write_to(&mut w).expect("write a frame");
        w.flush().ok();
    }

    /// Send `PaneType`; its request id.
    pub(crate) fn type_in(&mut self, bytes: &[u8], paste: bool, enter: bool, only_at_shell: bool, watch: Option<WatchBody>) -> u32 {
        let req_id = self.next_req;
        self.next_req += 1;
        let t = PaneType { req_id, sid: 0, paste, enter, only_at_shell, bytes: bytes.to_vec(), watch };
        self.send(MsgType::PaneType, encode_pane_type(&t).expect("encode PaneType"));
        req_id
    }

    /// Send `PaneWatch`; its request id.
    pub(crate) fn watch(&mut self, watch: WatchBody) -> u32 {
        let req_id = self.next_req;
        self.next_req += 1;
        let w = PaneWatch { req_id, sid: 0, watch };
        self.send(MsgType::PaneWatch, encode_pane_watch(&w).expect("encode PaneWatch"));
        req_id
    }

    pub(crate) fn inject(&mut self, text: &str) {
        self.send(MsgType::InjectInput, marspot_term::shell_proto::encode_inject_input(0, text.as_bytes()));
    }

    fn read_one(&mut self) {
        let mut r = &self.stream;
        match Frame::read_from(&mut r) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::PaneTyped => self.got.push_back(Reply::Typed(decode_pane_typed(&f.payload).unwrap())),
                MsgType::PaneDone => self.got.push_back(Reply::Done(decode_pane_done(&f.payload).unwrap())),
                MsgType::PaneWatchFired => {
                    self.got.push_back(Reply::Fired(decode_pane_watch_fired(&f.payload).unwrap()))
                }
                _ => {}
            },
            Ok(None) => panic!("the session closed its control socket"),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("reading the control socket: {e}"),
        }
    }

    /// The first answer `pick` takes, within `secs`; the others stay.
    pub(crate) fn take<T>(&mut self, secs: f64, what: &str, mut pick: impl FnMut(&Reply) -> Option<T>) -> T {
        self.try_take(secs, &mut pick).unwrap_or_else(|| panic!("no {what} within {secs}s; got {:?}", self.got))
    }

    pub(crate) fn try_take<T>(&mut self, secs: f64, mut pick: impl FnMut(&Reply) -> Option<T>) -> Option<T> {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            if let Some(i) = self.got.iter().position(|r| pick(r).is_some()) {
                let r = self.got.remove(i).unwrap();
                return pick(&r);
            }
            if Instant::now() >= until {
                return None;
            }
            self.read_one();
        }
    }

    pub(crate) fn typed(&mut self, req_id: u32, secs: f64) -> PaneTyped {
        self.take(secs, &format!("PaneTyped {req_id}"), |r| match r {
            Reply::Typed(t) if t.req_id == req_id => Some(*t),
            _ => None,
        })
    }

    pub(crate) fn done(&mut self, req_id: u32) -> PaneDone {
        self.take(5.0, &format!("PaneDone {req_id}"), |r| match r {
            Reply::Done(d) if d.req_id == req_id => Some(d.clone()),
            _ => None,
        })
    }

    pub(crate) fn fired(&mut self, watch_id: u32, secs: f64) -> PaneWatchFired {
        self.try_fired(watch_id, secs).unwrap_or_else(|| panic!("watch {watch_id} did not fire within {secs}s"))
    }

    pub(crate) fn try_fired(&mut self, watch_id: u32, secs: f64) -> Option<PaneWatchFired> {
        self.try_take(secs, |r| match r {
            Reply::Fired(f) if f.watch_id == watch_id => Some(*f),
            _ => None,
        })
    }

    /// Register `body` and expect it taken.
    pub(crate) fn armed(&mut self, body: WatchBody) {
        let req = self.watch(body);
        let d = self.done(req);
        assert_eq!(d.code, DoneCode::Ok, "{d:?}");
    }

    pub(crate) fn cancel(&mut self, watch_id: u32) {
        let req = self.watch(body(watch_id, WatchKind::Cancel, 0));
        assert_eq!(self.done(req).code, DoneCode::Ok);
    }

    /// The output position now: an empty `PaneType` answers with it.
    pub(crate) fn pos(&mut self) -> u64 {
        let req = self.type_in(b"", false, false, false, None);
        let t = self.typed(req, 5.0);
        assert_eq!(t.result, TypedResult::Written);
        t.pos
    }
}

pub(crate) fn body(watch_id: u32, kind: WatchKind, since_pos: u64) -> WatchBody {
    WatchBody { watch_id, kind, since_pos, quiet_ms: 0, text: String::new() }
}

pub(crate) fn text(watch_id: u32, since_pos: u64, text: &str) -> WatchBody {
    WatchBody { text: text.into(), ..body(watch_id, WatchKind::Text, since_pos) }
}

/// A pane running `script` (a `/bin/sh` script body) in place of a shell.
pub(crate) fn with_script(sandbox: &mut marspot_term::test_env::StateSandbox, body: &str) -> Session {
    use std::os::unix::fs::PermissionsExt;
    let script = sandbox.dir.join("pane.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write the script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);
    let s = Session::spawn(bin(), COLS, ROWS);
    screen_shows(&s, "READY");
    s
}

/// A pane with zsh, without the user's startup files, at its prompt.
pub(crate) fn with_zsh(sandbox: &mut marspot_term::test_env::StateSandbox) -> Session {
    let shell = marspot_term::test_env::shell_without_rc_files(sandbox.dir.path(), "/bin/zsh");
    sandbox.env.set("SHELL", &shell);
    let s = Session::spawn(bin(), COLS, ROWS);
    wait(10.0, "the shell's prompt", || s.try_screen_and_cursor(COLS, ROWS).is_some_and(|(_, (c, _))| c > 0));
    s
}

pub(crate) fn bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_marspot-session"))
}

pub(crate) fn wait(secs: f64, what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    while !f() {
        assert!(Instant::now() < until, "timed out after {secs}s waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn screen_shows(s: &Session, text: &str) {
    wait(10.0, text, || s.try_screen(COLS, ROWS).is_some_and(|rows| rows.iter().any(|r| r.contains(text))));
}

/// Where `text` is on the published screen, numbered absolutely (the
/// frame's scroll-push count plus the row).
pub(crate) fn abs_row_of(s: &Session, text: &str) -> Option<u64> {
    let mut cells = Vec::new();
    let snap = s.reader.read(&mut cells, &mut Vec::new())?;
    let rows = support_rows(&cells);
    let r = rows.iter().position(|l| l.contains(text))?;
    Some(snap.scroll_push_count + r as u64)
}

fn support_rows(cells: &[marspot_term::grid::Cell]) -> Vec<String> {
    cells.chunks(COLS as usize).take(ROWS as usize).map(|row| row.iter().map(|c| c.ch).collect()).collect()
}

/// The scroll-push count of the latest frame.
pub(crate) fn pushed(s: &Session) -> u64 {
    let mut cells = Vec::new();
    s.reader.read(&mut cells, &mut Vec::new()).map_or(0, |f| f.scroll_push_count)
}
