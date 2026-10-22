//! Frames on a Claude Code inbox socket: one JSON object per line.
//!
//! What a Claude Code 2.1.306 sender actually writes (captured on a
//! listening socket):
//!
//! ```text
//! {"msgV":1,"msg_id":"…","type":"user","message":{"role":"user","content":"<envelope>"},
//!  "priority":"next","from":"uds:/tmp/cc-socks/88641.sock"}
//! ```
//!
//! It sends no auth line, and before a message it may connect and hang
//! up without writing anything (a liveness check).  Unknown types and
//! actions are skipped, never treated as an error, so a newer sender
//! does not break an older reader.

use crate::json::{self, Value, obj};

/// Longest line accepted.  The sender caps a message at about a
/// million characters; CJK text is three bytes each in UTF-8.
pub const MAX_LINE: usize = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Interrupt the current turn.
    Now,
    /// Next in line.
    Next,
    /// When the session is otherwise idle.
    Later,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Now => "now",
            Priority::Next => "next",
            Priority::Later => "later",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "now" => Some(Priority::Now),
            "next" => Some(Priority::Next),
            "later" => Some(Priority::Later),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserFrame {
    /// Usually an envelope (see `envelope`).
    pub content: String,
    pub from: Option<String>,
    pub priority: Priority,
    pub msg_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    Auth,
    User(UserFrame),
    Control { action: String, frame: Value },
    /// A type this reader does not know.
    Other,
}

pub fn parse_line(line: &str) -> Result<Inbound, String> {
    let v = json::parse(line)?;
    match v.str_at("type") {
        Some("auth") => Ok(Inbound::Auth),
        Some("user") => {
            let content = v.at(&["message", "content"]).and_then(Value::str).filter(|c| !c.is_empty());
            let content = content.ok_or("user frame without text content")?.to_string();
            Ok(Inbound::User(UserFrame {
                content,
                from: v.str_at("from").map(str::to_string),
                priority: v.str_at("priority").and_then(Priority::parse).unwrap_or(Priority::Next),
                msg_id: v.str_at("msg_id").map(str::to_string),
            }))
        }
        Some("control") => {
            let action = v.str_at("action").unwrap_or_default().to_string();
            Ok(Inbound::Control { action, frame: v })
        }
        _ => Ok(Inbound::Other),
    }
}

/// One user frame, newline-terminated.
pub fn user_line(content: &str, from: &str, priority: Priority, msg_id: &str) -> String {
    let v = obj([
        ("msgV", Value::from(1)),
        ("msg_id", Value::from(msg_id)),
        ("type", Value::from("user")),
        ("message", obj([("role", Value::from("user")), ("content", Value::from(content))])),
        ("priority", Value::from(priority.as_str())),
        ("from", Value::from(from)),
    ]);
    let mut s = v.to_json();
    s.push('\n');
    s
}

/// What happened to a message, told back to its sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receipt {
    Delivered,
    Refused,
}

/// A `peer_message_status` control frame, newline-terminated.  The
/// sender shows it to its model, so a message that did not arrive is
/// not mistaken for one that did.  Claude Code encodes "refused" as
/// status `expired` with detail `refused`; this follows it.
pub fn receipt_line(r: Receipt, from: &str, orig_msg_id: Option<&str>, reason: &str) -> String {
    let mut kv = vec![
        ("type".to_string(), Value::from("control")),
        ("action".to_string(), Value::from("peer_message_status")),
    ];
    match r {
        Receipt::Delivered => kv.push(("status".into(), Value::from("delivered"))),
        Receipt::Refused => {
            kv.push(("status".into(), Value::from("expired")));
            kv.push(("status_detail".into(), Value::from("refused")));
        }
    }
    kv.push(("reason".into(), Value::from(reason)));
    kv.push(("from".into(), Value::from(from)));
    if let Some(id) = orig_msg_id {
        kv.push(("orig_msg_id".into(), Value::from(id)));
    }
    let mut s = Value::Obj(kv).to_json();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line Claude Code 2.1.306 wrote to a listening socket.
    #[test]
    fn reads_a_captured_claude_frame() {
        let line = r#"{"msgV":1,"msg_id":"6cea2483-4e51-477f-9d85-0262f7b5ec7c","type":"user","message":{"role":"user","content":"<cross-session-message from=\"uds:/tmp/cc-socks/88641.sock\" from-name=\"marspot-1e\" from-mode=\"bypass\">\nProbe\n</cross-session-message>"},"priority":"next","from":"uds:/tmp/cc-socks/88641.sock"}"#;
        let Inbound::User(f) = parse_line(line).unwrap() else { panic!() };
        assert_eq!(f.priority, Priority::Next);
        assert_eq!(f.from.as_deref(), Some("uds:/tmp/cc-socks/88641.sock"));
        assert_eq!(f.msg_id.as_deref(), Some("6cea2483-4e51-477f-9d85-0262f7b5ec7c"));
        let env = crate::envelope::parse(&f.content).unwrap();
        assert_eq!(env.from_mode, Some(crate::envelope::Mode::Bypass));
    }

    #[test]
    fn written_frames_read_back() {
        let line = user_line("<x>\n\"q\"", "uds:/tmp/cc-socks/1.sock", Priority::Now, "id-1");
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        let Inbound::User(f) = parse_line(line.trim_end()).unwrap() else { panic!() };
        assert_eq!(f.content, "<x>\n\"q\"");
        assert_eq!(f.priority, Priority::Now);
    }

    #[test]
    fn unknown_and_control_frames_are_not_errors() {
        assert_eq!(parse_line(r#"{"type":"auth","token":"t"}"#).unwrap(), Inbound::Auth);
        assert_eq!(parse_line(r#"{"type":"future_thing"}"#).unwrap(), Inbound::Other);
        let Inbound::Control { action, .. } = parse_line(r#"{"type":"control","action":"notify_when_idle"}"#).unwrap()
        else {
            panic!()
        };
        assert_eq!(action, "notify_when_idle");
    }

    #[test]
    fn a_user_frame_without_text_is_an_error() {
        assert!(parse_line(r#"{"type":"user","message":{"role":"user","content":""}}"#).is_err());
        assert!(parse_line(r#"{"type":"user","message":{"content":[{"type":"text"}]}}"#).is_err());
        assert!(parse_line("not json").is_err());
    }

    #[test]
    fn refused_receipt_uses_the_expired_encoding() {
        let line = receipt_line(Receipt::Refused, "uds:/a.sock", Some("m1"), "why");
        let v = json::parse(line.trim_end()).unwrap();
        assert_eq!(v.str_at("status"), Some("expired"));
        assert_eq!(v.str_at("status_detail"), Some("refused"));
        assert_eq!(v.str_at("orig_msg_id"), Some("m1"));
        let d = json::parse(receipt_line(Receipt::Delivered, "uds:/a.sock", None, "ok").trim_end()).unwrap();
        assert_eq!(d.str_at("status"), Some("delivered"));
        assert_eq!(d.get("orig_msg_id"), None);
    }
}
