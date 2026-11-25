//! Sending a message to a claude session's inbox, the way the quota
//! heads-up does: from `marspot`, read after the session's current
//! step, asking for no reply.  Called on the watch's own thread.

use std::path::PathBuf;

use crate::plugins::claudecode::heads_up::{frame_line, inbox_of};

/// `body` to the inbox of the claude with this pid.
pub(super) fn deliver(claude_pid: i32, body: &str) -> Result<(), String> {
    let inbox = inbox_of(claude_pid).ok_or_else(|| "the session has no inbox".to_string())?;
    send(&marspot_peer::registry::default_dirs(), &inbox, body)
}

/// `body` to `inbox`, refused when the records in `dirs` that name it
/// are all of a process that is gone.
fn send(dirs: &[PathBuf], inbox: &str, body: &str) -> Result<(), String> {
    marspot_peer::inbox::write_in(dirs, inbox, &frame_line(inbox, body))
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::net::UnixListener;

    use marspot_peer::envelope::{self, Mode};
    use marspot_peer::frame::{self, Priority};

    use super::*;

    #[test]
    fn zombie_message_reaches_the_inbox_from_marspot() {
        let dir = std::env::temp_dir().join(format!("zombie-inbox-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("1.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let inbox = sock.display().to_string();
        let me = std::process::id() as i32;
        let rec = format!("{{\"pid\":{me},\"procStart\":\"{}\",\"messagingSocketPath\":\"{inbox}\"}}", marspot_peer::proc::lstart(me).unwrap());
        std::fs::write(dir.join(format!("{me}.json")), rec).unwrap();
        let body = "marspot 提醒：程序检查发现这个会话名下有 1 项疑似已经没用的工作。\n\n1. 任务 b1";
        send(std::slice::from_ref(&dir), &inbox, body).unwrap();
        let (conn, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        let frame::Inbound::User(f) = frame::parse_line(line.trim_end()).unwrap() else { panic!("{line}") };
        assert_eq!(f.priority, Priority::Next);
        let env = envelope::parse(&f.content).expect("an envelope the receiver accepts");
        assert_eq!((env.from_name.as_deref(), env.from_mode), (Some("marspot"), Some(Mode::Bypass)));
        assert_eq!(env.body, body);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
