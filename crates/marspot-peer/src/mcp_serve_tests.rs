use super::*;
use crate::json;
use std::io::{BufReader, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// What the stand-in app saw.
#[derive(Debug, PartialEq)]
enum Seen {
    /// A wait came in and is held, unanswered.
    Holding,
    /// A held wait's caller closed its connection: a write to it fails.
    Closed,
}

/// A stand-in app: a `pane.read` is held until its caller hangs up;
/// `pane.list` is answered at once with `pad` bytes of filler.
fn app(pad: usize) -> (PathBuf, Receiver<Seen>) {
    let path = std::env::temp_dir().join(format!("mcp-serve-{}.sock", crate::rand::hex(4)));
    let l = UnixListener::bind(&path).unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let mut c = c.unwrap();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line.contains("\"pane.read\"") {
                    tx.send(Seen::Holding).unwrap();
                    let mut rest = Vec::new();
                    let _ = r.read_to_end(&mut rest);
                    // the app watches the write side: only a socket the
                    // caller closed, not one it shut down, fails a write
                    while c.write_all(b"\n").is_ok() {}
                    tx.send(Seen::Closed).unwrap();
                } else {
                    let pad = "x".repeat(pad);
                    let _ = c.write_all(format!("{{\"ok\":true,\"result\":{{\"panes\":[],\"pad\":\"{pad}\"}}}}\n").as_bytes());
                }
            });
        }
    });
    (path, rx)
}

struct Client {
    to: UnixStream,
    from: BufReader<UnixStream>,
    serving: std::thread::JoinHandle<()>,
}

fn start(path: PathBuf) -> Client {
    let (to, input) = UnixStream::pair().unwrap();
    let (output, from) = UnixStream::pair().unwrap();
    let serving = std::thread::spawn(move || serve(Server::at(path, None).waiting(Duration::from_millis(50)), BufReader::new(input), output));
    let mut c = Client { to, from: BufReader::new(from), serving };
    c.send(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":"codex-mcp-client"}}}"#);
    assert_eq!(c.recv().get("id").and_then(Value::i64), Some(0));
    c
}

impl Client {
    fn send(&mut self, line: &str) {
        writeln!(self.to, "{line}").unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.from.read_line(&mut line).unwrap();
        json::parse(&line).unwrap_or_else(|_| panic!("not one JSON line: {line:?}"))
    }

    fn call(&mut self, id: i64, tool: &str, args: &str) {
        self.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#));
    }

    /// Close stdin; every line written after, to the end.
    fn finish(mut self) -> Vec<Value> {
        self.to.shutdown(std::net::Shutdown::Write).unwrap();
        self.serving.join().unwrap();
        let mut rest = String::new();
        self.from.read_to_string(&mut rest).unwrap();
        rest.lines().map(|l| json::parse(l).unwrap()).collect()
    }
}

const WAIT: &str = r#"{"target":"p","wait":{"change":true},"timeout_s":600}"#;

fn next(rx: &Receiver<Seen>) -> Seen {
    rx.recv_timeout(Duration::from_secs(10)).unwrap()
}

#[test]
fn mcp_a_long_wait_does_not_hold_up_other_calls_and_a_cancel_ends_it() {
    let (path, seen) = app(0);
    let mut c = start(path.clone());
    c.call(1, "PaneRead", WAIT);
    assert_eq!(next(&seen), Seen::Holding);
    c.call(2, "Panes", "{}");
    let r = c.recv();
    assert_eq!(r.get("id").and_then(Value::i64), Some(2));
    assert_eq!(r.at(&["result", "isError"]).and_then(Value::bool), Some(false), "{}", r.to_json());
    c.call(3, "ListAgents", "{}");
    assert_eq!(c.recv().get("id").and_then(Value::i64), Some(3));
    c.send(r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#);
    assert_eq!(c.recv().get("id").and_then(Value::i64), Some(4));

    c.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"user"}}"#);
    assert_eq!(next(&seen), Seen::Closed);
    let rest = c.finish();
    let _ = std::fs::remove_file(&path);
    assert!(rest.is_empty(), "a cancelled call was answered: {rest:?}");
}

#[test]
fn mcp_calls_past_the_cap_are_refused_and_stdin_closing_cancels_the_rest() {
    let (path, seen) = app(0);
    let mut c = start(path.clone());
    for id in 1..=MAX_IN_FLIGHT as i64 {
        c.call(id, "PaneRead", WAIT);
    }
    for _ in 0..MAX_IN_FLIGHT {
        assert_eq!(next(&seen), Seen::Holding);
    }
    c.call(99, "PaneRead", WAIT);
    let r = c.recv();
    assert_eq!(r.get("id").and_then(Value::i64), Some(99));
    assert_eq!(r.at(&["error", "code"]).and_then(Value::i64), Some(-32000));
    let rest = c.finish();
    for _ in 0..MAX_IN_FLIGHT {
        assert_eq!(next(&seen), Seen::Closed);
    }
    let _ = std::fs::remove_file(&path);
    assert!(rest.is_empty(), "{rest:?}");
}

/// A one-shot client writes its call and closes stdin at once.
#[test]
fn mcp_a_call_that_is_not_a_wait_is_answered_after_stdin_closes() {
    let (path, seen) = app(0);
    let mut c = start(path.clone());
    c.call(1, "PaneRead", WAIT);
    assert_eq!(next(&seen), Seen::Holding);
    c.call(2, "Panes", "{}");
    c.call(3, "SendMessage", r#"{"to":"x","message":"hi"}"#);
    let rest = c.finish();
    assert_eq!(next(&seen), Seen::Closed);
    let _ = std::fs::remove_file(&path);
    let mut ids: Vec<i64> = rest.iter().filter_map(|r| r.get("id").and_then(Value::i64)).collect();
    ids.sort();
    assert_eq!(ids, [2, 3], "{rest:?}");
}

#[test]
fn mcp_answers_written_at_once_never_interleave() {
    let (path, _seen) = app(200 * 1024);
    let mut c = start(path.clone());
    let n = 12;
    for id in 1..=n {
        c.call(id, "Panes", "{}");
    }
    let mut ids: Vec<i64> = (0..n).map(|_| c.recv().get("id").and_then(Value::i64).unwrap()).collect();
    ids.sort();
    assert_eq!(ids, (1..=n).collect::<Vec<_>>());
    assert!(c.finish().is_empty());
    let _ = std::fs::remove_file(&path);
}
