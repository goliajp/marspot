//! The one place a frame is written into an agent session's inbox.
//!
//! Every message, receipt, idle subscription and notice handed to
//! another session ends here, whoever sent it: the command line, the
//! MCP server, a relay, or marspot itself.  The recipient is a model
//! that acts on what it reads, so right before connecting, every time,
//! the socket is looked up in the session registry and the process the
//! record names is checked to be the one that wrote the record, by pid
//! and start time.  A socket file outlives its session, and its path
//! can be bound again by whoever comes next.
//!
//! A socket no record names here is written as before: a Claude session
//! can keep its registry in a directory these do not cover, and its
//! reply address is still the one it gave.  There is no identity to
//! check it against, so none is checked.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::frame;
use crate::registry::{self, Entry};

/// How long a recipient gets to take a frame.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Write `line`, one frame with or without its newline, into the inbox
/// at `socket`.
pub fn write(socket: &str, line: &str) -> Result<(), String> {
    write_in(&registry::default_dirs(), socket, line)
}

/// [`write`], with the registry read from `dirs`.
pub fn write_in(dirs: &[PathBuf], socket: &str, line: &str) -> Result<(), String> {
    let mut buf = line.trim_end_matches('\n').to_string();
    if buf.contains('\n') || buf.len() > frame::MAX_LINE {
        return Err("not one frame".into());
    }
    buf.push('\n');
    check_owner(dirs, socket)?;
    let mut s = marspot_term::uds_path::connect(socket).map_err(|e| e.to_string())?;
    let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
    s.write_all(buf.as_bytes()).map_err(|e| e.to_string())
}

/// `Err` when records name `socket` and none of the processes they name
/// is still the one that wrote it.
fn check_owner(dirs: &[PathBuf], socket: &str) -> Result<(), String> {
    let named: Vec<Entry> = dirs
        .iter()
        .flat_map(|d| registry::read_all(d))
        .filter(|e| e.socket.as_deref() == Some(socket))
        .collect();
    if named.is_empty() || named.iter().any(Entry::is_live) {
        return Ok(());
    }
    let pids = named.iter().map(|e| e.pid.to_string()).collect::<Vec<_>>().join(", ");
    Err(format!("the session that owned {socket} (pid {pids}) is gone or its pid was reused"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    fn dir() -> PathBuf {
        let d = PathBuf::from(format!("/tmp/mpi-{}", crate::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(dir: &Path, pid: i32, proc_start: &str, socket: &Path) {
        let text = format!(
            "{{\"pid\":{pid},\"sessionId\":\"s\",\"procStart\":\"{proc_start}\",\"messagingSocketPath\":\"{}\"}}",
            socket.display()
        );
        std::fs::write(dir.join(format!("{pid}.json")), text).unwrap();
    }

    fn read_one(listener: &UnixListener) -> String {
        let (conn, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        line
    }

    /// A socket whose record names a live process, with the start time
    /// it has, takes the frame, newline-terminated, once.
    #[test]
    fn a_live_session_gets_the_frame() {
        let d = dir();
        let sock = d.join("1.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let me = std::process::id() as i32;
        record(&d, me, &crate::proc::lstart(me).unwrap(), &sock);
        write_in(std::slice::from_ref(&d), sock.to_str().unwrap(), "{\"a\":1}").unwrap();
        assert_eq!(read_one(&listener), "{\"a\":1}\n");
        write_in(std::slice::from_ref(&d), sock.to_str().unwrap(), "{\"b\":2}\n").unwrap();
        assert_eq!(read_one(&listener), "{\"b\":2}\n", "a newline already there is not doubled");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Nothing is written to a socket whose record names a process that
    /// is not the one that wrote it: the listener is never even
    /// connected to.
    #[test]
    fn a_socket_whose_owner_is_gone_is_not_written() {
        let d = dir();
        let sock = d.join("1.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        // this process, but started at another time: a reused pid
        let me = std::process::id() as i32;
        record(&d, me, "Thu Jan  1 00:00:00 1970", &sock);
        let err = write_in(std::slice::from_ref(&d), sock.to_str().unwrap(), "{}").unwrap_err();
        assert!(err.contains("is gone or its pid was reused"), "{err}");
        assert!(listener.accept().is_err(), "nothing connected");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A socket no record here names has no identity to check, and is
    /// written as it always was.
    #[test]
    fn a_socket_no_record_names_is_written_unchecked() {
        let d = dir();
        let sock = d.join("1.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        write_in(std::slice::from_ref(&d), sock.to_str().unwrap(), "{}").unwrap();
        assert_eq!(read_one(&listener), "{}\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_frames_in_one_write_are_refused() {
        let err = write_in(&[], "/nowhere.sock", "{}\n{}").unwrap_err();
        assert_eq!(err, "not one frame");
    }
}
