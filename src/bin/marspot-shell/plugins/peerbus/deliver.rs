//! What happens to a frame that reached a Codex inbox: whether it is
//! let in, what text Codex is given, and the receipt the sender gets.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use marspot_peer::envelope::{self, Mode};
use marspot_peer::frame::{self, DropReason, Receipt, UserFrame};
use marspot_term::lx_info;

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
    /// The receipt the sender gets.
    pub fn receipt(&self) -> Receipt {
        match self {
            Refusal::RateLimited => Receipt::Dropped(DropReason::RateLimited),
            Refusal::TooManyHops => Receipt::Dropped(DropReason::HopRunaway),
            Refusal::ClassMismatch { .. } => Receipt::Refused,
        }
    }

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
    // The envelope goes to Codex as it came, exactly what a Claude
    // session is shown: who it is from and where to answer.  How to
    // answer is in the SendMessage tool's description, as in Claude.
    Ok(Admitted { text: f.content.clone(), reply_to })
}

/// Tell a sender what happened.
pub(super) fn send_receipt(relay: &Path, to: &str, from: &str, r: Receipt, orig_msg_id: Option<&str>, reason: &str) {
    send_line(relay, to, &frame::receipt_line(r, from, orig_msg_id, reason));
}

/// One frame to a `uds:` address, through the session's relay.  Best
/// effort: a peer that has gone away has nobody left to tell.
pub(super) fn send_line(relay: &Path, to: &str, line: &str) {
    let Some(path) = to.strip_prefix("uds:") else { return };
    if let Err(e) = marspot_peer::relay::send_via(&relay.to_string_lossy(), path, line) {
        lx_info!("peerbus.send_failed", &e, to = to.to_string());
    }
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
    fn same_class_is_admitted_as_the_envelope_alone() {
        let f = frame_from(Some(Mode::Bypass), "do the thing");
        let a = admit(&f, Mode::Bypass, &mut Rate::default(), Instant::now()).unwrap();
        assert_eq!(a.text, f.content, "what a Claude session would be shown");
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

    /// Only statuses and drop reasons a Claude Code sender understands.
    #[test]
    fn refusals_map_to_claude_receipts() {
        let class = Refusal::ClassMismatch { sender: None, ours: Mode::Bypass };
        assert_eq!(class.receipt(), Receipt::Refused);
        assert_eq!(Refusal::RateLimited.receipt(), Receipt::Dropped(DropReason::RateLimited));
        assert_eq!(Refusal::TooManyHops.receipt(), Receipt::Dropped(DropReason::HopRunaway));
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
