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
    /// Switch the autorun policy on or off for a pane, or (with no
    /// target) report which panes it is on for.
    Autorun { target: String, on: Option<bool>, reply: Sender<Result<String, String>> },
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
/// Start the command socket.
///
/// `wake` is called after every request is queued.  Without it the
/// supervisor picks the request up on its next timer tick, so what the
/// user waits for is not the work but the tick -- and the tick is the
/// thing we want to slow down.  Queue, then knock.
pub type Wake = std::sync::Arc<dyn Fn() + Send + Sync>;

pub fn serve(tx: Sender<CliRequest>, wake: Wake) -> io::Result<()> {
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
                        let wake = wake.clone();
                        // A connection per thread: requests are rare,
                        // short, and must not be able to wedge each
                        // other or the accept loop.
                        std::thread::Builder::new()
                            .name("l1-cmd-conn".into())
                            .spawn(move || handle(stream, tx, wake))
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

fn handle(mut stream: UnixStream, tx: Sender<CliRequest>, wake: Wake) {
    // Queue, then knock.  The supervisor is asleep between ticks and
    // the caller is about to block for its answer.
    let send = |req: CliRequest| -> bool {
        let queued = tx.send(req).is_ok();
        if queued {
            wake();
        }
        queued
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let frame = match Frame::read_from(&mut stream) {
        Ok(Some(f)) => f,
        _ => return,
    };
    if frame.msg_type == MsgType::CliListPanes {
        let (rtx, rrx) = std::sync::mpsc::channel();
        if send(CliRequest::ListPanes { reply: rtx })
            && let Ok(panes) = rrx.recv_timeout(std::time::Duration::from_secs(5)) {
                let _ = Frame::new(
                    MsgType::CliPaneList,
                    marspot_term::shell_proto::encode_cli_pane_list(&panes),
                )
                .write_to(&mut stream);
                return;
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
        if !send(CliRequest::ReadPane { target, extra_lines, reply: rtx }) {
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
    if frame.msg_type == MsgType::CliAutorun {
        let Ok((target, on)) = marspot_term::shell_proto::decode_cli_autorun(&frame.payload) else {
            let _ = Frame::new(MsgType::CliResult, encode_cli_result(false, "bad request"))
                .write_to(&mut stream);
            return;
        };
        let (rtx, rrx) = std::sync::mpsc::channel();
        let sent = send(CliRequest::Autorun { target, on, reply: rtx });
        let out = if sent {
            rrx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| Err("shell did not answer in 5 s".into()))
        } else {
            Err("shell is shutting down".into())
        };
        let frame = match out {
            Ok(text) => Frame::new(
                MsgType::CliText,
                marspot_term::shell_proto::encode_cli_text(&text),
            ),
            Err(e) => Frame::new(MsgType::CliResult, encode_cli_result(false, &e)),
        };
        let _ = frame.write_to(&mut stream);
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
    if !send(CliRequest::SendText { target, text, reply: rtx }) {
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

/// Where the autorun opt-in lives.
///
/// One working directory per line.  A directory rather than a name or
/// an id: names change when a twin appears, ids change when a session
/// is rebuilt, but "the pane for this project" is what a person means
/// when they switch the thing on.
pub fn autorun_path() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("autorun.tsv")
}

pub fn load_autorun() -> std::collections::HashSet<String> {
    std::fs::read_to_string(autorun_path())
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn save_autorun(on: &std::collections::HashSet<String>) -> io::Result<()> {
    let mut lines: Vec<&String> = on.iter().collect();
    lines.sort();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let path = autorun_path();
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
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

/// Client half: switch the autorun policy for a pane, or ask which
/// panes have it on (`target` empty, `on` None).
pub fn autorun(target: &str, on: Option<bool>) -> io::Result<Result<String, String>> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Frame::new(
        MsgType::CliAutorun,
        marspot_term::shell_proto::encode_cli_autorun(target, on),
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
