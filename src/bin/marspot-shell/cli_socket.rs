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
pub struct CliRequest {
    pub target: String,
    pub text: String,
    pub reply: Sender<(bool, String)>,
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
    if tx.send(CliRequest { target, text, reply: rtx }).is_err() {
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

/// Resolve a pane by the name a person would use for it.
///
/// Panes do not have names; they have working directories, and what a
/// person calls a pane is the last component of one ("spg").  Matching
/// is exact on that component first, then unique-substring — an
/// ambiguous name is an error rather than a guess, because the cost of
/// guessing wrong is text typed into someone else's session.
pub fn resolve_pane(target: &str, panes: &[(u64, String)]) -> Result<u64, String> {
    let needle = target.trim().to_lowercase();
    if needle.is_empty() {
        return Err("empty target".into());
    }
    let base = |cwd: &str| -> String {
        cwd.rsplit('/').next().unwrap_or(cwd).to_lowercase()
    };
    let exact: Vec<u64> = panes
        .iter()
        .filter(|(_, cwd)| base(cwd) == needle)
        .map(|(sid, _)| *sid)
        .collect();
    let candidates = if exact.is_empty() {
        panes
            .iter()
            .filter(|(_, cwd)| cwd.to_lowercase().contains(&needle))
            .map(|(sid, _)| *sid)
            .collect()
    } else {
        exact
    };
    match candidates.len() {
        0 => Err(format!("no pane matches {target:?}")),
        1 => Ok(candidates[0]),
        n => Err(format!("{target:?} matches {n} panes; be more specific")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes() -> Vec<(u64, String)> {
        vec![
            (390, "/Users/doracawl/workspace/goliajp/spg".into()),
            (382, "/Users/doracawl/workspace/goliajp/marspot".into()),
            (384, "/Users/doracawl/workspace/stables/goliajp".into()),
            (386, "/Users/doracawl".into()),
        ]
    }

    /// The name a person uses is the last path component.
    #[test]
    fn a_pane_is_found_by_its_project_name() {
        assert_eq!(resolve_pane("spg", &panes()), Ok(390));
        assert_eq!(resolve_pane("SPG", &panes()), Ok(390), "case is not a distinction");
        assert_eq!(resolve_pane(" marspot ", &panes()), Ok(382));
    }

    /// An exact directory name beats a substring, so a pane can always
    /// be addressed by its own name even when another path contains it.
    #[test]
    fn an_exact_name_wins_over_a_path_that_merely_contains_it() {
        // "goliajp" is a directory of its own AND a path component of
        // two others; the pane actually called that is the answer.
        assert_eq!(resolve_pane("goliajp", &panes()), Ok(384));
    }

    /// Ambiguity is an error, never a guess: the cost of guessing is
    /// text typed into someone else's session.
    #[test]
    fn an_ambiguous_or_missing_name_is_refused() {
        let ps = vec![
            (1, "/w/alpha-one".to_string()),
            (2, "/w/alpha-two".to_string()),
        ];
        assert!(resolve_pane("alpha", &ps).unwrap_err().contains("2 panes"));
        assert!(resolve_pane("nope", &ps).unwrap_err().contains("no pane"));
        assert!(resolve_pane("  ", &ps).is_err());
    }
}
