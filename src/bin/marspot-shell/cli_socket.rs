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

/// Give every pane a name, unique by construction.
///
/// The name is the working directory's last component.  When several
/// panes share one, **all of them** get a `#k` — not just the extras —
/// so a name never silently means "the first one".  `k` ranks by
/// session id, which is creation order, so the numbering is stable
/// while the set is; close one and the rest renumber on the next
/// listing, which is what "no renaming, no duplicates, automatic"
/// means.  Nothing is stored: the names are derived every time, so
/// they cannot go stale.
pub fn assign_names(panes: &[(u64, String)]) -> Vec<(u64, String)> {
    let base = |cwd: &str| -> String {
        let b = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if b.is_empty() { "?".to_string() } else { b.to_string() }
    };
    let mut by_base: std::collections::HashMap<String, Vec<u64>> = std::collections::HashMap::new();
    for (sid, cwd) in panes {
        by_base.entry(base(cwd)).or_default().push(*sid);
    }
    for sids in by_base.values_mut() {
        sids.sort_unstable();
    }
    panes
        .iter()
        .map(|(sid, cwd)| {
            let b = base(cwd);
            let peers = &by_base[&b];
            let name = if peers.len() == 1 {
                b
            } else {
                let k = peers.iter().position(|s| s == sid).unwrap_or(0) + 1;
                format!("{b}#{k}")
            };
            (*sid, name)
        })
        .collect()
}

/// Resolve a name or a path tail against the panes.
///
/// Names first (they are unique by construction), then a path tail —
/// `goliajp/spg` matches `/w/goliajp/spg` but not `/w/goliajp/spg-old`,
/// so adding a parent always narrows — then unique substring.
///
/// Ambiguity is never resolved by guessing: the cost of guessing wrong
/// is text typed into someone else's session.  The error names the
/// candidates with their ids and names, so the next attempt is a
/// copy-paste rather than an investigation.
pub fn resolve_name(target: &str, panes: &[(u64, String)]) -> Result<u64, String> {
    let needle = target.trim().to_lowercase();
    let named = assign_names(panes);
    if let Some((sid, _)) = named.iter().find(|(_, n)| n.to_lowercase() == needle) {
        return Ok(*sid);
    }
    let norm = |s: &str| s.trim_end_matches('/').to_lowercase();
    let candidates: Vec<&(u64, String)> = if needle.contains('/') {
        panes
            .iter()
            .filter(|(_, cwd)| {
                let c = norm(cwd);
                c == needle || c.ends_with(&format!("/{needle}"))
            })
            .collect()
    } else {
        let hit: Vec<&(u64, String)> = panes
            .iter()
            .filter(|(_, cwd)| norm(cwd).rsplit('/').next().unwrap_or("") == needle)
            .collect();
        if hit.is_empty() {
            panes.iter().filter(|(_, cwd)| norm(cwd).contains(&needle)).collect()
        } else {
            hit
        }
    };
    match candidates.len() {
        0 => Err(format!("no pane matches {target:?}")),
        1 => Ok(candidates[0].0),
        _ => {
            let name_of = |sid: u64| -> String {
                named
                    .iter()
                    .find(|(s, _)| *s == sid)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default()
            };
            let list = candidates
                .iter()
                .map(|(sid, cwd)| format!("  {sid}  {:<20}  {cwd}", name_of(*sid)))
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

    /// Names are automatic, unique, and derived — never stored.
    #[test]
    fn duplicate_names_all_get_a_number_not_just_the_extras() {
        let ps = vec![
            (390, "/w/goliajp/spg".to_string()),
            (412, "/w/stables/spg".to_string()),
            (382, "/w/goliajp/marspot".to_string()),
        ];
        let names: std::collections::HashMap<u64, String> =
            assign_names(&ps).into_iter().collect();
        // Both, not "spg" and "spg#2": a name must never quietly mean
        // "whichever one came first".
        assert_eq!(names[&390], "spg#1");
        assert_eq!(names[&412], "spg#2");
        assert_eq!(names[&382], "marspot", "a name with no rival keeps it");
        // Rank is by session id, i.e. creation order — stable while the
        // set is.
        assert!(390 < 412);
    }

    /// Close one and the numbering follows, with no state to update.
    #[test]
    fn closing_a_pane_renumbers_the_rest() {
        let three = vec![
            (10, "/w/a/dup".to_string()),
            (20, "/w/b/dup".to_string()),
            (30, "/w/c/dup".to_string()),
        ];
        let names: std::collections::HashMap<u64, String> =
            assign_names(&three).into_iter().collect();
        assert_eq!((&names[&10], &names[&20], &names[&30]), (&"dup#1".into(), &"dup#2".into(), &"dup#3".into()));

        let two: Vec<(u64, String)> = three.into_iter().filter(|(s, _)| *s != 20).collect();
        let names: std::collections::HashMap<u64, String> = assign_names(&two).into_iter().collect();
        assert_eq!(names[&10], "dup#1");
        assert_eq!(names[&30], "dup#2", "the survivors close the gap");

        // And a name that is no longer shared loses its number.
        let one: Vec<(u64, String)> = two.into_iter().filter(|(s, _)| *s == 10).collect();
        assert_eq!(assign_names(&one)[0].1, "dup");
    }

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

    /// A `#k` name is exact: it must not fall through to the substring
    /// match and pick up the other one.
    #[test]
    fn a_numbered_name_addresses_exactly_one_pane() {
        let ps = vec![
            (390, "/w/goliajp/spg".to_string()),
            (412, "/w/stables/spg".to_string()),
        ];
        assert_eq!(resolve_name("spg#1", &ps), Ok(390));
        assert_eq!(resolve_name("spg#2", &ps), Ok(412));
        // The bare name belongs to neither now, and says so with both.
        let e = resolve_name("spg", &ps).unwrap_err();
        assert!(e.contains("390") && e.contains("412") && e.contains("spg#1"), "{e}");
    }

    /// The name a person uses is the last path component.
    #[test]
    fn a_pane_is_found_by_its_project_name() {
        assert_eq!(resolve_name("spg", &panes()), Ok(390));
        assert_eq!(resolve_name("SPG", &panes()), Ok(390), "case is not a distinction");
        assert_eq!(resolve_name(" marspot ", &panes()), Ok(382));
    }

    /// An exact directory name beats a substring, so a pane can always
    /// be addressed by its own name even when another path contains it.
    #[test]
    fn an_exact_name_wins_over_a_path_that_merely_contains_it() {
        // "goliajp" is a directory of its own AND a path component of
        // two others; the pane actually called that is the answer.
        assert_eq!(resolve_name("goliajp", &panes()), Ok(384));
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
        let e = resolve_name("alpha", &ps).unwrap_err();
        assert!(e.contains("2 panes"), "{e}");
        // Id, name and directory: everything the next attempt needs.
        assert!(e.contains("1  alpha-one") && e.contains("/w/alpha-one"), "{e}");
        assert!(e.contains("2  alpha-two") && e.contains("/w/alpha-two"), "{e}");
        assert!(resolve_name("nope", &ps).unwrap_err().contains("no pane"));
        assert!(resolve_name("  ", &ps).is_err());
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
        let e = resolve_name("spg", &ps).unwrap_err();
        assert!(e.contains("390") && e.contains("412"), "{e}");
        assert!(!e.contains("413"), "a different directory is not a candidate: {e}");

        // A path tail narrows, and narrows *exactly*: `goliajp/spg`
        // must not also match `goliajp/spg-old`.
        assert_eq!(resolve_name("goliajp/spg", &ps), Ok(390));
        assert_eq!(resolve_name("stables/spg", &ps), Ok(412));

        // Ids are an address in their own right, recognised before any
        // name matching happens — see `parse_target`.
        assert_eq!(parse_target("412"), Ok(Target::Id(412)));
        assert_eq!(parse_target("#390"), Ok(Target::Id(390)));
    }
}
