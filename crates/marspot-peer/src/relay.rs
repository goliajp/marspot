//! `marspot-peer relay`: the process that stands for one Codex session
//! on the inbox sockets.
//!
//! Claude Code checks who is on the other end of an inbox socket: an
//! idle subscription is sent only to a socket whose listening process
//! is the pid in the recipient's registry record, and an idle notice
//! goes back only to a socket served by the process that sent the
//! subscription.  A Codex session's inbox is served by marspot, not by
//! Codex, so one small process per session takes that place: it
//! listens on the public inbox and passes every connection through to
//! marspot, and it makes every connection the session opens to a peer.
//! The registry record names this process.
//!
//! It lives as long as its standard input: marspot holds the other end
//! of that pipe, and when marspot goes, so does every relay.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::frame;
use crate::json::{self, Value, obj};

/// Connections served at once, per socket.
const MAX_CONNS: usize = 8;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a peer gets to take a frame.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve until standard input closes.  `public` is the inbox peers
/// write to, `upstream` where marspot reads what arrives, `send` where
/// this session hands over frames for peers.
pub fn run(public: &Path, upstream: &Path, send: &Path) -> io::Result<()> {
    let inbox = bind(public)?;
    let outbox = bind(send)?;
    let up = upstream.to_path_buf();
    spawn("relay-inbox", move || serve(inbox, move |c| pass_through(c, &up)))?;
    spawn("relay-send", move || serve(outbox, send_for))?;
    // blocks until marspot closes the pipe, or exits
    let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
    let _ = std::fs::remove_file(public);
    let _ = std::fs::remove_file(send);
    Ok(())
}

fn bind(path: &Path) -> io::Result<UnixListener> {
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).map(drop)
}

fn serve(listener: UnixListener, handle: impl Fn(UnixStream) + Send + Sync + Clone + 'static) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if active.load(Ordering::SeqCst) >= MAX_CONNS {
            continue; // dropped: the other end sees the connection close
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (active, handle) = (active.clone(), handle.clone());
        let spawned = spawn("relay-conn", move || {
            handle(conn);
            active.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            continue;
        }
    }
}

/// Copy what a peer writes to marspot.  An empty connection (a
/// liveness check) is passed on as one too.
fn pass_through(mut conn: UnixStream, upstream: &Path) {
    let _ = conn.set_read_timeout(Some(IO_TIMEOUT));
    let Ok(mut up) = UnixStream::connect(upstream) else { return };
    let _ = up.set_write_timeout(Some(IO_TIMEOUT));
    let _ = io::copy(&mut (&mut conn).take(frame::MAX_LINE as u64 * 4), &mut up);
}

/// A request to send, one JSON object per line:
/// `{"to":"/tmp/cc-socks/1.sock","line":"{…}"}`, answered `ok` or
/// `error: …` on a line of its own.
fn send_for(conn: UnixStream) {
    let _ = conn.set_read_timeout(Some(IO_TIMEOUT));
    let Ok(mut out) = conn.try_clone() else { return };
    let mut reader = BufReader::new((&conn).take(u64::MAX));
    let mut line = String::new();
    loop {
        line.clear();
        reader.get_mut().set_limit(frame::MAX_LINE as u64 * 2);
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let answer = match deliver(line.trim_end()) {
            Ok(()) => "ok\n".to_string(),
            Err(e) => format!("error: {e}\n"),
        };
        if out.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

fn deliver(request: &str) -> Result<(), String> {
    let v = json::parse(request)?;
    let to = v.str_at("to").ok_or("no `to`")?;
    let line = v.str_at("line").ok_or("no `line`")?;
    if line.contains('\n') || line.len() > frame::MAX_LINE {
        return Err("not one frame".into());
    }
    let mut s = UnixStream::connect(to).map_err(|e| e.to_string())?;
    let _ = s.set_write_timeout(Some(SEND_TIMEOUT));
    s.write_all(line.as_bytes()).and_then(|()| s.write_all(b"\n")).map_err(|e| e.to_string())
}

/// Hand `line` (one frame, with or without its newline) to the relay at
/// `relay` for delivery to the socket at `to`.
pub fn send_via(relay: &str, to: &str, line: &str) -> Result<(), String> {
    let mut s = UnixStream::connect(relay).map_err(|e| format!("this session's relay is not running: {e}"))?;
    let _ = s.set_read_timeout(Some(SEND_TIMEOUT + Duration::from_secs(5)));
    let req = obj([("to", Value::from(to)), ("line", Value::from(line.trim_end_matches('\n')))]).to_json();
    s.write_all(req.as_bytes()).and_then(|()| s.write_all(b"\n")).map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new(&s).read_line(&mut answer).map_err(|e| e.to_string())?;
    match answer.trim_end() {
        "ok" => Ok(()),
        "" => Err("the relay closed without answering".into()),
        other => Err(other.strip_prefix("error: ").unwrap_or(other).to_string()),
    }
}

/// marspot's private sockets for relay `key`, under `dir`: where the
/// relay passes inbound connections, and where it takes sends.
pub fn private_paths(dir: &Path, key: u64) -> (PathBuf, PathBuf) {
    (dir.join(format!("{key}-in.sock")), dir.join(format!("{key}-send.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = PathBuf::from(format!("/tmp/mpr-{}", crate::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A frame through the inbox reaches upstream byte for byte, and a
    /// send request reaches its target, from the relay's own process.
    #[test]
    fn frames_pass_both_ways() {
        let d = dir();
        let (public, upstream, send, target) = (d.join("pub.sock"), d.join("up.sock"), d.join("send.sock"), d.join("t.sock"));
        let up = UnixListener::bind(&upstream).unwrap();
        let tl = UnixListener::bind(&target).unwrap();
        let (inbox, outbox) = (bind(&public).unwrap(), bind(&send).unwrap());
        let u = upstream.clone();
        spawn("t-in", move || serve(inbox, move |c| pass_through(c, &u))).unwrap();
        spawn("t-out", move || serve(outbox, send_for)).unwrap();

        let mut c = UnixStream::connect(&public).unwrap();
        c.write_all(b"{\"type\":\"user\"}\n").unwrap();
        drop(c);
        let (mut got, _) = up.accept().unwrap();
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "{\"type\":\"user\"}\n");

        send_via(send.to_str().unwrap(), target.to_str().unwrap(), "{\"a\":1}\n").unwrap();
        let (mut got, _) = tl.accept().unwrap();
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "{\"a\":1}\n");

        let err = send_via(send.to_str().unwrap(), d.join("nobody.sock").to_str().unwrap(), "{}").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(std::fs::metadata(&public).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
