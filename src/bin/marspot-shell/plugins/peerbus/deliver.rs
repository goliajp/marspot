//! What happens to a frame that reached a Codex inbox: whether it is
//! let in, what text Codex is given, and the receipt the sender gets.

use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use marspot_peer::envelope::{self, Mode};
use marspot_peer::frame::{self, Receipt, UserFrame};

/// Messages one Codex session accepts per minute.
const RATE_PER_MIN: usize = 30;
/// Longest relay chain accepted.
const MAX_HOPS: usize = 8;

/// Why a frame was not let in, worded for the sender's model.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    RateLimited,
    TooManyHops,
    /// The sender did not declare its permission class, or declared a
    /// different one.  Claude Code holds such a message for its user;
    /// a Codex session has no way to ask, so it is refused.
    ClassMismatch { sender: Option<Mode>, ours: Mode },
}

impl Refusal {
    pub fn reason(&self) -> String {
        match self {
            Refusal::RateLimited => format!("recipient accepts at most {RATE_PER_MIN} messages a minute"),
            Refusal::TooManyHops => format!("relayed through more than {MAX_HOPS} sessions"),
            Refusal::ClassMismatch { sender, ours } => format!(
                "recipient runs in the {} permission class and the message declared {}",
                ours.as_str(),
                sender.map_or("none", Mode::as_str)
            ),
        }
    }
}

/// Sliding one-minute window.
#[derive(Default)]
pub(super) struct Rate {
    recent: VecDeque<Instant>,
}

impl Rate {
    pub fn admit(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            self.recent.pop_front();
        }
        if self.recent.len() >= RATE_PER_MIN {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// A frame that passed the checks, ready for Codex.
#[derive(Debug, PartialEq)]
pub(super) struct Admitted {
    pub text: String,
    /// Where the receipt goes, when the sender gave a usable address.
    pub reply_to: Option<String>,
}

/// The sender's reply address: the envelope's, else the frame's.
pub(super) fn reply_address(f: &UserFrame) -> Option<String> {
    envelope::parse(&f.content)
        .and_then(|e| e.from)
        .or_else(|| f.from.clone())
        .filter(|a| a.starts_with("uds:") && envelope::is_address(a))
}

pub(super) fn admit(f: &UserFrame, ours: Mode, rate: &mut Rate, now: Instant) -> Result<Admitted, Refusal> {
    let env = envelope::parse(&f.content);
    let sender = env.as_ref().and_then(|e| e.from_mode);
    if sender != Some(ours) {
        return Err(Refusal::ClassMismatch { sender, ours });
    }
    if env.as_ref().and_then(|e| e.hop_chain.as_ref()).is_some_and(|h| h.len() > MAX_HOPS) {
        return Err(Refusal::TooManyHops);
    }
    if !rate.admit(now) {
        return Err(Refusal::RateLimited);
    }
    let reply_to = reply_address(f);
    // The envelope goes to Codex as it came: it says who the message is
    // from, and the TUI shows it to the user that way.  The last line
    // is how Codex answers; it does not otherwise know the command.
    let mut text = f.content.clone();
    if let Some(to) = &reply_to {
        text.push_str(&format!("\n(Reply with: marspot-peer send {to} \"<text>\")"));
    }
    Ok(Admitted { text, reply_to })
}

/// Tell a sender what happened.  Best effort with a short timeout: a
/// sender that has gone away has nobody left to tell.
pub(super) fn send_receipt(to: &str, from: &str, r: Receipt, orig_msg_id: Option<&str>, reason: &str) {
    let Some(path) = to.strip_prefix("uds:") else { return };
    let Ok(mut s) = UnixStream::connect(path) else { return };
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = s.write_all(frame::receipt_line(r, from, orig_msg_id, reason).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::envelope::Header;
    use marspot_peer::frame::Priority;

    fn frame_from(mode: Option<Mode>, body: &str) -> UserFrame {
        let h = Header { from: "uds:/tmp/cc-socks/9.sock".into(), from_name: Some("luna-37".into()), from_mode: mode };
        UserFrame { content: envelope::encode(&h, body), from: Some("uds:/tmp/cc-socks/9.sock".into()), priority: Priority::Next, msg_id: Some("m".into()) }
    }

    #[test]
    fn same_class_is_admitted_with_a_reply_line() {
        let f = frame_from(Some(Mode::Bypass), "do the thing");
        let a = admit(&f, Mode::Bypass, &mut Rate::default(), Instant::now()).unwrap();
        assert!(a.text.starts_with(&f.content));
        assert!(a.text.ends_with("\n(Reply with: marspot-peer send uds:/tmp/cc-socks/9.sock \"<text>\")"));
        assert_eq!(a.reply_to.as_deref(), Some("uds:/tmp/cc-socks/9.sock"));
    }

    #[test]
    fn missing_or_other_class_is_refused() {
        let mut rate = Rate::default();
        for (m, ours) in [(None, Mode::Bypass), (Some(Mode::Prompting), Mode::Bypass), (Some(Mode::Bypass), Mode::Prompting)] {
            let err = admit(&frame_from(m, "x"), ours, &mut rate, Instant::now()).unwrap_err();
            assert_eq!(err, Refusal::ClassMismatch { sender: m, ours });
        }
        let plain = UserFrame { content: "no envelope".into(), from: None, priority: Priority::Next, msg_id: None };
        assert!(admit(&plain, Mode::Bypass, &mut rate, Instant::now()).is_err());
    }

    #[test]
    fn the_rate_window_slides() {
        let mut rate = Rate::default();
        let t0 = Instant::now();
        for _ in 0..RATE_PER_MIN {
            assert!(rate.admit(t0));
        }
        assert!(!rate.admit(t0 + Duration::from_secs(59)));
        assert!(rate.admit(t0 + Duration::from_secs(60)));
    }

    #[test]
    fn long_relay_chains_are_refused() {
        let hop = "0123456789abcdef01234567";
        let chain = [hop; MAX_HOPS + 1].join(",");
        let content = format!("<cross-session-message from=\"uds:/a.sock\" hop-chain=\"{chain}\" from-mode=\"bypass\">\nb\n</cross-session-message>");
        let f = UserFrame { content, from: None, priority: Priority::Next, msg_id: None };
        assert_eq!(admit(&f, Mode::Bypass, &mut Rate::default(), Instant::now()), Err(Refusal::TooManyHops));
    }
}
