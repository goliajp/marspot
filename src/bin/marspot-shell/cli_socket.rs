//! L1's command socket — how something outside the app asks a pane to
//! be typed into.
//!
//! Why through L1 rather than straight to the session: a fresh client
//! on L3's own socket *replaces* the control writer L2 is using, so a
//! CLI that connected, spoke and hung up would leave that pane with no
//! poke channel until the core reconnected — the pane stops repainting.
//! L1 is also where the one queue lives that keeps two scripts from
//! typing into the same PTY at once, and a request from outside is
//! exactly the kind that arrives at an awkward moment.
//!
//! The socket is inside the state dir, so its permissions are the
//! directory's: this is a same-user channel, not an authenticated one.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::Sender;

use marspot_term::shell_proto::{
    decode_cli_send_text, encode_cli_result, Frame, MsgType,
};
use marspot_term::{lx_info, lx_warn};

/// One request, with the way to answer it.
///
/// The reply channel is part of the request because the answer is not
/// "we received it" — it is "this pane, or none, and why", which only
/// the main loop can say.
pub enum CliRequest {
    /// Type text into a pane and press Enter.
    SendText { target: String, text: String, reply: Sender<(bool, String)> },
    /// What panes are there?  Answered from L1's own view, not the
    /// caller's: one source of truth for what a name means.
    ListPanes { reply: Sender<Vec<(u64, String, String)>> },
    /// What does this pane say?  `Ok(text)` or a reason.
    ReadPane { target: String, extra_lines: u32, reply: Sender<Result<String, String>> },
}

pub fn socket_path() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("l1-cmd.sock")
}

/// Bind the socket and serve it on a thread.
///
/// Best-effort by design: a shell that cannot bind still runs the
/// terminal.  The stale-socket unlink is unconditional because the
/// previous owner is this same process's predecessor — an execv, a
/// crash — and there is no case where a live one should be preserved.
pub fn serve(tx: Sender<CliRequest>) -> io::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new()
        .name("l1-cmd".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let tx = tx.clone();
                        // A connection per thread: requests are rare,
                        // short, and must not be able to wedge each
                        // other or the accept loop.
                        std::thread::Builder::new()
                            .name("l1-cmd-conn".into())
                            .spawn(move || handle(stream, tx))
                            .ok();
                    }
                    Err(e) => {
                        lx_warn!("shell.cli.accept_failed", &format!("{e}"));
                        break;
                    }
                }
            }
        })?;
    lx_info!("shell.cli.listening", "command socket bound", path = path.display().to_string());
    Ok(())
}

fn handle(mut stream: UnixStream, tx: Sender<CliRequest>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let frame = match Frame::read_from(&mut stream) {
        Ok(Some(f)) => f,
        _ => return,
    };
    if frame.msg_type == MsgType::CliListPanes {
        let (rtx, rrx) = std::sync::mpsc::channel();
        if tx.send(CliRequest::ListPanes { reply: rtx }).is_ok() {
            if let Ok(panes) = rrx.recv_timeout(std::time::Duration::from_secs(5)) {
                let _ = Frame::new(
                    MsgType::CliPaneList,
                    marspot_term::shell_proto::encode_cli_pane_list(&panes),
                )
                .write_to(&mut stream);
                return;
            }
        }
        let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, "no answer"))
            .write_to(&mut stream);
        return;
    }
    if frame.msg_type == MsgType::CliReadPane {
        let reply_err = |stream: &mut UnixStream, msg: &str| {
            let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, msg))
                .write_to(stream);
        };
        let Ok((target, extra_lines)) =
            marspot_term::shell_proto::decode_cli_read_pane(&frame.payload)
        else {
            reply_err(&mut stream, "bad request");
            return;
        };
        let (rtx, rrx) = std::sync::mpsc::channel();
        if tx.send(CliRequest::ReadPane { target, extra_lines, reply: rtx }).is_err() {
            reply_err(&mut stream, "shell is shutting down");
            return;
        }
        match rrx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(text)) => {
                let _ = Frame::new(
                    MsgType::CliText,
                    marspot_term::shell_proto::encode_cli_text(&text),
                )
                .write_to(&mut stream);
            }
            Ok(Err(e)) => reply_err(&mut stream, &e),
            Err(_) => reply_err(&mut stream, "shell did not answer in 5 s"),
        }
        return;
    }
    if frame.msg_type != MsgType::CliSendText {
        let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, "unknown request"))
            .write_to(&mut stream);
        return;
    }
    let (target, text) = match decode_cli_send_text(&frame.payload) {
        Ok(v) => v,
        Err(e) => {
            let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, &format!("{e}")))
                .write_to(&mut stream);
            return;
        }
    };
    let (rtx, rrx) = std::sync::mpsc::channel();
    if tx.send(CliRequest::SendText { target, text, reply: rtx }).is_err() {
        let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, "shell is shutting down"))
            .write_to(&mut stream);
        return;
    }
    // The main loop answers on its next pass; a caller that waited
    // forever for a wedged loop would be worse than one told to retry.
    let (ok, msg) = rrx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or((false, "shell did not answer in 5 s".into()));
    let _ = Frame::new(MsgType::CliResult, encode_cli_result(ok, &msg)).write_to(&mut stream);
}

/// Client half: ask a running shell what panes it has.
pub fn list_panes() -> io::Result<Vec<(u64, String, String)>> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Frame::new(MsgType::CliListPanes, Vec::new()).write_to(&mut stream)?;
    match Frame::read_from(&mut stream)? {
        Some(f) if f.msg_type == MsgType::CliPaneList => {
            marspot_term::shell_proto::decode_cli_pane_list(&f.payload)
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "no pane list")),
    }
}

/// Client half: ask a running shell what a pane says.
pub fn read_pane(target: &str, extra_lines: u32) -> io::Result<Result<String, String>> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Frame::new(
        MsgType::CliReadPane,
        marspot_term::shell_proto::encode_cli_read_pane(target, extra_lines),
    )
    .write_to(&mut stream)?;
    match Frame::read_from(&mut stream)? {
        Some(f) if f.msg_type == MsgType::CliText => {
            Ok(Ok(marspot_term::shell_proto::decode_cli_text(&f.payload)?))
        }
        Some(f) if f.msg_type == MsgType::CliResult => {
            Ok(Err(marspot_term::shell_proto::decode_cli_result(&f.payload)?.1))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "no reply")),
    }
}

/// Client half: ask a running shell to type into a pane.
pub fn send_text(target: &str, text: &str) -> io::Result<(bool, String)> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Frame::new(
        MsgType::CliSendText,
        marspot_term::shell_proto::encode_cli_send_text(target, text),
    )
    .write_to(&mut stream)?;
    match Frame::read_from(&mut stream)? {
        Some(f) if f.msg_type == MsgType::CliResult => {
            marspot_term::shell_proto::decode_cli_result(&f.payload)
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "no reply")),
    }
}

/// How a caller said which pane it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The session id — the one address that is never reused and never
    /// moves.  Everything else is a way of not having to know it.
    Id(u64),
    /// A display name (`spg`, `doracawl#2`) or a path tail
    /// (`goliajp/spg`).
    Name(String),
    /// The cell a pane occupies: window `w`, column `x`, row `y`, all
    /// 1-based because a person counting panes starts at one.  Survives
    /// a rename (there are none) but not a rearrangement — which is the
    /// point: it addresses *the pane in that slot*, whichever it is.
    Cell { w: usize, x: usize, y: usize },
}

/// Parse `w(2,1,3)` / `390` / `#390` / `spg` / `goliajp/spg`.
pub fn parse_target(target: &str) -> Result<Target, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("empty target".into());
    }
    if let Some(rest) = t.strip_prefix('w').or_else(|| t.strip_prefix('W')) {
        if let Some(inner) = rest.trim().strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
            let nums: Vec<&str> = inner.split(',').map(str::trim).collect();
            if nums.len() != 3 {
                return Err(format!("{target:?}: expected w(window, x, y)"));
            }
            let mut vals = [0usize; 3];
            for (i, n) in nums.iter().enumerate() {
                vals[i] = n
                    .parse()
                    .map_err(|_| format!("{target:?}: {n:?} is not a number"))?;
                if vals[i] == 0 {
                    return Err(format!("{target:?}: counts start at 1"));
                }
            }
            return Ok(Target::Cell { w: vals[0], x: vals[1], y: vals[2] });
        }
    }
    let bare = t.strip_prefix('#').unwrap_or(t);
    if let Ok(sid) = bare.parse::<u64>() {
        return Ok(Target::Id(sid));
    }
    Ok(Target::Name(t.to_string()))
}

/// Naming and name resolution live in the library — L1 resolves
/// `--send spg#2` and L2 draws the name on the pane, and two
/// implementations of a naming rule is two naming rules.
pub use marspot::pane_name::{assign as assign_names, resolve as resolve_name};

#[cfg(test)]
mod tests {
    use super::*;

    /// The three ways of saying which pane.
    #[test]
    fn every_address_form_parses() {
        assert_eq!(parse_target("390"), Ok(Target::Id(390)));
        assert_eq!(parse_target("#390"), Ok(Target::Id(390)));
        assert_eq!(parse_target("spg#2"), Ok(Target::Name("spg#2".into())));
        assert_eq!(parse_target("goliajp/spg"), Ok(Target::Name("goliajp/spg".into())));
        assert_eq!(parse_target("w(2,1,3)"), Ok(Target::Cell { w: 2, x: 1, y: 3 }));
        assert_eq!(parse_target(" W( 1 , 2 , 2 ) "), Ok(Target::Cell { w: 1, x: 2, y: 2 }));
        // Counting starts at one, so zero is a mistake worth naming.
        assert!(parse_target("w(0,1,1)").is_err());
        assert!(parse_target("w(1,1)").is_err());
        assert!(parse_target("").is_err());
    }

    // The naming rules themselves are the library's, and tested there
    // (`marspot::pane_name`).  What belongs in this module is the
    // parsing of the three address forms.
}
