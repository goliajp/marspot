//! The asking side of the control socket, for the command line and the
//! MCP tools.
//!
//! Retries are limited to what cannot do anything twice: an app that
//! cannot be reached, or a core that is restarting, is asked again for
//! up to [`RETRY_FOR`]; a connection lost after a request went out is
//! asked again only for a read.  Sending `pane.open` twice could open
//! two panes.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{ErrorCode, LineRead, Op, Request, Response, read_line, socket_path};

pub const RETRY_FOR: Duration = Duration::from_secs(3);
const RETRY_STEP: Duration = Duration::from_millis(200);

#[derive(Debug, PartialEq)]
pub enum CallError {
    /// No app answered on the socket.
    Unreachable(String),
    /// The connection went after the request was sent: it may have been
    /// carried out.
    Lost,
}

/// How long to wait for the answer to `op`: its own deadline and some.
fn answer_within(op: &Op) -> Duration {
    let wait = match op {
        Op::Run(r) => r.timeout_s,
        Op::Read(r) if r.wait.is_some() => r.timeout_s,
        _ => 0,
    };
    Duration::from_secs(u64::from(wait) + 30)
}

/// Ends a call from another thread: the connection it has open is shut
/// and closed, and it opens no other.  The app ends a wait whose caller
/// closed its connection.
#[derive(Default)]
pub struct Cancel(Mutex<(bool, Option<UnixStream>)>);

impl Cancel {
    pub fn cancel(&self) {
        let mut g = self.0.lock().unwrap();
        g.0 = true;
        if let Some(s) = g.1.take() {
            // wakes the calling thread's read; it then drops its own handle
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Keep a handle on `s` for [`Cancel::cancel`]; false once cancelled.
    fn hold(&self, s: &UnixStream) -> Result<bool, std::io::Error> {
        let mut g = self.0.lock().unwrap();
        if g.0 {
            return Ok(false);
        }
        g.1 = Some(s.try_clone()?);
        Ok(true)
    }
}

fn once(path: &Path, req: &Request, line: &str, cancel: &Cancel) -> Result<Response, Attempt> {
    let mut s = UnixStream::connect(path).map_err(|e| Attempt::Connect(e.to_string()))?;
    if !cancel.hold(&s).map_err(|e| Attempt::Connect(e.to_string()))? {
        return Err(Attempt::Cancelled);
    }
    s.set_read_timeout(Some(answer_within(&req.op))).map_err(|e| Attempt::Connect(e.to_string()))?;
    s.write_all(line.as_bytes()).map_err(|_| Attempt::Lost)?;
    let mut r = BufReader::new(s);
    match read_line(&mut r) {
        Ok(LineRead::Line(l)) => Response::parse(&l).map_err(|_| Attempt::Lost),
        _ => Err(Attempt::Lost),
    }
}

enum Attempt {
    Connect(String),
    Lost,
    Cancelled,
}

/// Send `req` and wait for its answer, retrying as the module says.
pub fn call(req: &Request) -> Result<Response, CallError> {
    call_at(&socket_path(), req)
}

pub fn call_at(path: &Path, req: &Request) -> Result<Response, CallError> {
    call_cancellable(path, req, &Cancel::default())
}

/// [`call_at`], ended early by `cancel`; a cancelled call is [`CallError::Lost`].
pub fn call_cancellable(path: &Path, req: &Request, cancel: &Cancel) -> Result<Response, CallError> {
    let line = format!("{}\n", req.to_json());
    let until = Instant::now() + RETRY_FOR;
    loop {
        let pause = match once(path, req, &line, cancel) {
            Ok(Response::Err(r)) if r.code == ErrorCode::CoreRestarting && Instant::now() < until => {
                Duration::from_millis(r.retry_after_ms.unwrap_or(200)).min(RETRY_STEP * 5)
            }
            Ok(resp) => return Ok(resp),
            Err(Attempt::Connect(e)) if Instant::now() >= until => return Err(CallError::Unreachable(e)),
            Err(Attempt::Connect(_)) => RETRY_STEP,
            Err(Attempt::Lost) if req.op.is_read_only() && Instant::now() < until => RETRY_STEP,
            Err(Attempt::Lost | Attempt::Cancelled) => return Err(CallError::Lost),
        };
        std::thread::sleep(pause.min(until.saturating_duration_since(Instant::now())).max(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    fn sock(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mpc-{tag}-{}.sock", crate::rand::hex(4)))
    }

    #[test]
    fn pane_control_unreachable_after_retrying() {
        let t = Instant::now();
        let e = call_at(&sock("none"), &Request { op: Op::List, caller_thread: None }).unwrap_err();
        assert!(matches!(e, CallError::Unreachable(_)));
        assert!(t.elapsed() >= RETRY_FOR);
    }

    /// A server that hangs up on the first request and answers the next.
    fn flaky(path: &Path) -> std::thread::JoinHandle<usize> {
        let l = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut seen = 0;
            for conn in l.incoming().take(2) {
                let mut c = conn.unwrap();
                let mut line = String::new();
                BufReader::new(&c).read_line(&mut line).unwrap();
                seen += 1;
                if seen == 2 {
                    c.write_all(b"{\"ok\":true,\"result\":{}}\n").unwrap();
                }
            }
            seen
        })
    }

    #[test]
    fn pane_control_a_lost_read_is_asked_again() {
        let p = sock("read");
        let h = flaky(&p);
        let r = call_at(&p, &Request { op: Op::List, caller_thread: None }).unwrap();
        assert!(matches!(r, Response::Ok(_)));
        assert_eq!(h.join().unwrap(), 2);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pane_control_a_lost_change_is_not_sent_twice() {
        let p = sock("close");
        let l = UnixListener::bind(&p).unwrap();
        let h = std::thread::spawn(move || {
            {
                let (c, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&c).read_line(&mut line).unwrap();
            }
            l.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            l.accept().is_err()
        });
        let e = call_at(&p, &Request { op: Op::Close { target: "x".into() }, caller_thread: None }).unwrap_err();
        assert_eq!(e, CallError::Lost);
        assert!(h.join().unwrap(), "a second connection was made");
        let _ = std::fs::remove_file(&p);
    }
}
