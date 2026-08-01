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
/// Panes have no names; they have working directories, and what a
/// person calls a pane is the last component of one ("spg").  Three
/// ways to say which one, in order of how specific they are:
///
/// - **a session id** (`390`) — always unambiguous, and what the error
///   below hands back so a caller can retry without thinking;
/// - **a path tail** (`goliajp/spg`) — for when two panes share a last
///   component, which is the normal state of affairs once the same
///   project exists in two trees;
/// - **a bare name** (`spg`) — exact on the last component first, then
///   unique substring.
///
/// Ambiguity is never resolved by guessing: the cost of guessing wrong
/// is text typed into someone else's session.  The error names the
/// candidates *with their ids*, so the next attempt is a copy-paste
/// rather than an investigation.
pub fn resolve_pane(target: &str, panes: &[(u64, String)]) -> Result<u64, String> {
    let needle = target.trim().trim_start_matches('#').to_lowercase();
    if needle.is_empty() {
        return Err("empty target".into());
    }
    // A session id addresses exactly one pane, always.
    if let Ok(sid) = needle.parse::<u64>() {
        return if panes.iter().any(|(s, _)| *s == sid) {
            Ok(sid)
        } else {
            Err(format!("no pane with session id {sid}"))
        };
    }
    let norm = |s: &str| s.trim_end_matches('/').to_lowercase();
    let candidates: Vec<&(u64, String)> = if needle.contains('/') {
        // A path tail: `goliajp/spg` matches `/w/goliajp/spg` but not
        // `/w/goliajp/spg-old`, so adding a parent always narrows.
        panes
            .iter()
            .filter(|(_, cwd)| {
                let c = norm(cwd);
                c == needle || c.ends_with(&format!("/{needle}"))
            })
            .collect()
    } else {
        let base = |cwd: &str| -> String { norm(cwd).rsplit('/').next().unwrap_or("").to_string() };
        let exact: Vec<&(u64, String)> =
            panes.iter().filter(|(_, cwd)| base(cwd) == needle).collect();
        if exact.is_empty() {
            panes.iter().filter(|(_, cwd)| norm(cwd).contains(&needle)).collect()
        } else {
            exact
        }
    };
    match candidates.len() {
        0 => Err(format!("no pane matches {target:?}")),
        1 => Ok(candidates[0].0),
        _ => {
            let list = candidates
                .iter()
                .map(|(sid, cwd)| format!("  {sid}  {cwd}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "{target:?} matches {} panes — say which:\n{list}",
                candidates.len()
            ))
        }
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
    /// text typed into someone else's session.  The error has to be
    /// actionable, so it names the candidates *with their ids*.
    #[test]
    fn an_ambiguous_or_missing_name_is_refused_with_the_candidates() {
        let ps = vec![
            (1, "/w/alpha-one".to_string()),
            (2, "/w/alpha-two".to_string()),
        ];
        let e = resolve_pane("alpha", &ps).unwrap_err();
        assert!(e.contains("2 panes"), "{e}");
        assert!(e.contains("1  /w/alpha-one") && e.contains("2  /w/alpha-two"), "{e}");
        assert!(resolve_pane("nope", &ps).unwrap_err().contains("no pane"));
        assert!(resolve_pane("  ", &ps).is_err());
    }

    /// Two panes with the same project name is the normal state once
    /// the same project exists in two trees.  Both are addressable.
    #[test]
    fn same_named_panes_are_told_apart_by_path_or_by_id() {
        let ps = vec![
            (390, "/Users/x/workspace/goliajp/spg".to_string()),
            (412, "/Users/x/workspace/stables/spg".to_string()),
            (413, "/Users/x/workspace/goliajp/spg-old".to_string()),
        ];
        // The bare name is refused — and says which two.
        let e = resolve_pane("spg", &ps).unwrap_err();
        assert!(e.contains("390") && e.contains("412"), "{e}");
        assert!(!e.contains("413"), "a different directory is not a candidate: {e}");

        // A path tail narrows, and narrows *exactly*: `goliajp/spg`
        // must not also match `goliajp/spg-old`.
        assert_eq!(resolve_pane("goliajp/spg", &ps), Ok(390));
        assert_eq!(resolve_pane("stables/spg", &ps), Ok(412));

        // The id always works, and is what the error above hands back.
        assert_eq!(resolve_pane("412", &ps), Ok(412));
        assert_eq!(resolve_pane("#390", &ps), Ok(390));
        assert!(resolve_pane("999", &ps).unwrap_err().contains("999"));
    }
}
