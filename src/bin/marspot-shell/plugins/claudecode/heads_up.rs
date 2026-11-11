//! Telling a busy session its account is about to run out, while the
//! account can still answer.
//!
//! Once an account refuses, the claude on it cannot send a request,
//! so anything it is told then goes unanswered and the wait before a
//! move buys nothing but a pane standing still. The last of the week
//! is the only time a session can still act on the news: finish the
//! step it is on, start nothing long, write down what runs in the
//! background. The message goes to the session's own inbox, never
//! into the composer, so what the person is typing is not touched.

use std::collections::HashMap;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marspot_peer::envelope::{self, Header, Mode};
use marspot_peer::frame::{self, Priority};
use marspot_term::lx_info;

use super::quota::{self, Room};

/// What is left of the week, or of the pane's model cap, when the
/// session is told. The feed counts in whole percent, so this is the
/// last step before full.
const LAST_OF_WEEK: f64 = 0.01;
/// The same step before the five-hour window counts as spent.
const LAST_OF_FIVE_HOURS: f64 = 1.0 - quota::FIVE_HOUR_SPENT + 0.01;
/// The feed's figures are two-decimal fractions; `1.0 - 0.99` is a hair
/// over `0.01`.
const EPS: f64 = 1e-9;

/// Is this account, for a pane on `model`, one step from refusing it?
pub(in crate::plugins) fn nearly_out(room: &Room, model: Option<&str>) -> bool {
    if room.refused_for(model) {
        return false;
    }
    match room {
        Room::Known { .. } => {
            let (week, five_hours) = room.headroom_for(model);
            week <= LAST_OF_WEEK + EPS || five_hours <= LAST_OF_FIVE_HOURS + EPS
        }
        Room::Unknown => false,
    }
}

/// One warning, for one pane, about one account's week.
pub(in crate::plugins) struct Sent {
    pub(in crate::plugins) profile: u8,
    /// The week it was about, by when that week renews.
    pub(in crate::plugins) week: i64,
    /// Set once the session's inbox took the message. Until then the
    /// pane is treated as never told.
    pub(in crate::plugins) delivered: Arc<AtomicBool>,
}

impl Sent {
    pub(in crate::plugins) fn is_for(&self, profile: u8, week: i64) -> bool {
        self.profile == profile && self.week == week
    }
    pub(super) fn landed_on(&self, profile: u8) -> bool {
        self.profile == profile && self.delivered.load(Ordering::Relaxed)
    }
}

/// Every pane told, by session, kept on disk so a restart of the shell
/// -- every silent update is one -- neither tells a pane twice in a
/// week nor forgets that it was told, which is what lets it leave at
/// once when the account runs out.
#[derive(Default)]
pub(in crate::plugins) struct Ledger {
    sent: HashMap<u64, Sent>,
    /// What was last written, so a pass writes only on a change. The
    /// delivered flag is set by the sending thread, so a change can
    /// arrive between passes without the map itself being touched.
    written: String,
}

impl Deref for Ledger {
    type Target = HashMap<u64, Sent>;
    fn deref(&self) -> &Self::Target {
        &self.sent
    }
}

impl DerefMut for Ledger {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sent
    }
}

impl Ledger {
    /// One line per pane: `sid profile week delivered`, by sid.
    fn encode(&self) -> String {
        let mut sids: Vec<&u64> = self.sent.keys().collect();
        sids.sort();
        sids.iter()
            .map(|sid| {
                let s = &self.sent[sid];
                let delivered = u8::from(s.delivered.load(Ordering::Relaxed));
                format!("{sid}\t{}\t{}\t{delivered}\n", s.profile, s.week)
            })
            .collect()
    }

    /// Read back what `encode` wrote; a line that does not parse is
    /// dropped, and costs that pane at most a second message.
    pub(in crate::plugins) fn decode(text: &str) -> Self {
        let sent = text
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let sid = f.next()?.parse().ok()?;
                let profile = f.next()?.parse().ok()?;
                let week = f.next()?.parse().ok()?;
                let delivered = f.next()? == "1";
                Some((sid, Sent { profile, week, delivered: Arc::new(AtomicBool::new(delivered)) }))
            })
            .collect();
        let mut ledger = Self { sent, written: String::new() };
        ledger.written = ledger.encode();
        ledger
    }

    /// The text to write, when it differs from what was last written.
    pub(in crate::plugins) fn changed(&mut self) -> Option<String> {
        let now = self.encode();
        (now != self.written).then(|| {
            self.written.clone_from(&now);
            now
        })
    }
}

/// Said to a session whose account is nearly out. It arrives while
/// the session is working, so it asks for no stop and no reply.
pub(super) fn body(profile: u8) -> String {
    format!(
        "marspot 提醒：这个会话所用的账号（P{profile}）额度只剩不到 1%。用完后 marspot 会把会话换到另一个账号上重启，\
         重启会结束所有还在运行的后台任务。请照常继续工作，同时：把手头这一步做到可以中断的状态；不要再启动耗时长的后台任务；\
         正在运行的后台任务，把命令和输出位置记下来，重启后好接着做。这条消息不用回复。"
    )
}

/// Said after a move to a session that had work running in the
/// background when its claude was taken down.
pub(super) fn after_ended_work(resume_word: &str) -> String {
    format!("{resume_word}。换账号时还在运行的后台任务已经被结束了，请检查它们的状态，需要的话重新启动")
}

/// The frame for `inbox`, from marspot.
///
/// marspot acts for the person without asking first, which is what
/// `bypass` names, and the sessions it serves run the same way. The
/// reply address sits beside the inbox and nothing listens on it; the
/// message asks for no reply.
fn frame_line(inbox: &str, body: &str) -> String {
    let dir = Path::new(inbox).parent().map(|d| d.display().to_string()).unwrap_or_default();
    let from = format!("uds:{dir}/marspot-quota.sock");
    let header = Header { from: from.clone(), from_name: Some("marspot".into()), from_mode: Some(Mode::Bypass) };
    frame::user_line(&envelope::encode(&header, body), &from, Priority::Next, &marspot_peer::rand::uuid_v4())
}

fn deliver(inbox: &str, line: &str) -> std::io::Result<()> {
    let mut s = UnixStream::connect(inbox)?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(line.as_bytes())
}

/// The inbox of the claude with this pid, from the sessions registry.
fn inbox_of(claude_pid: i32) -> Option<String> {
    marspot_peer::peers::live_entries().into_iter().find(|e| e.pid == claude_pid && !e.ours)?.socket
}

/// Off the drawing thread: it reads the registry and writes a socket.
pub(super) fn send(sid: u64, claude_pid: i32, profile: u8, delivered: Arc<AtomicBool>) {
    send_to(sid, profile, delivered, "claudecode.quota.heads_up", move || inbox_of(claude_pid));
}

/// The same message for a session whose inbox is found by `inbox`.
pub(in crate::plugins) fn send_to(
    sid: u64,
    profile: u8,
    delivered: Arc<AtomicBool>,
    tag: &'static str,
    inbox: impl FnOnce() -> Option<String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = match inbox() {
            None => "the session has no inbox".to_string(),
            Some(inbox) => match deliver(&inbox, &frame_line(&inbox, &body(profile))) {
                Ok(()) => {
                    delivered.store(true, Ordering::Relaxed);
                    "delivered".to_string()
                }
                Err(e) => format!("not delivered: {e}"),
            },
        };
        lx_info!(tag, &format!("sid={sid} P{profile}: {outcome}"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    fn room(status: &str, util_5h: f64, util_7d: f64, fable: Option<f64>) -> Room {
        Room::Known {
            status: status.into(),
            util_5h,
            util_7d,
            reset_7d: 1000,
            models: fable.map(|u| vec![("fable".to_string(), u)]).unwrap_or_default(),
            models_measured_at: 0,
            measured_at: 0,
        }
    }

    #[test]
    fn the_last_percent_of_what_the_pane_uses_is_nearly_out() {
        assert!(!nearly_out(&room("allowed", 0.0, 0.98, None), None));
        assert!(nearly_out(&room("allowed", 0.0, 0.99, None), None));
        assert!(!nearly_out(&room("rejected", 0.0, 1.0, None), None), "out is not nearly out");
        // a Fable pane runs out with Fable; an Opus pane does not
        let fable_low = room("allowed", 0.0, 0.40, Some(0.99));
        assert!(nearly_out(&fable_low, Some("fable-5-1")));
        assert!(!nearly_out(&fable_low, Some("opus-5-5")));
        // the five hours, one step before they count as spent
        assert!(nearly_out(&room("allowed", 0.97, 0.10, None), None));
        assert!(!nearly_out(&room("allowed", 0.96, 0.10, None), None));
        assert!(!nearly_out(&Room::Unknown, None));
    }

    #[test]
    fn the_message_reaches_an_inbox_as_a_well_formed_envelope() {
        let dir = std::env::temp_dir().join(format!("hu-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let inbox = dir.join("1.sock");
        let listener = UnixListener::bind(&inbox).unwrap();
        let inbox = inbox.display().to_string();
        deliver(&inbox, &frame_line(&inbox, &body(8))).unwrap();
        let (conn, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        let frame::Inbound::User(f) = frame::parse_line(line.trim_end()).unwrap() else { panic!("{line}") };
        assert_eq!(f.priority, Priority::Next, "after the current step, never cutting the turn");
        let env = envelope::parse(&f.content).expect("an envelope the receiver accepts");
        assert_eq!(env.from_mode, Some(Mode::Bypass));
        assert_eq!(env.from_name.as_deref(), Some("marspot"));
        assert!(env.from.as_deref().is_some_and(|a| a.ends_with("/marspot-quota.sock")));
        assert!(env.body.contains("P8"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_ledger_survives_a_restart_and_is_written_only_on_a_change() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.changed(), None, "nothing told, nothing to write");
        let delivered = Arc::new(AtomicBool::new(false));
        ledger.insert(7, Sent { profile: 8, week: 1_794_373_200, delivered: Arc::clone(&delivered) });
        let first = ledger.changed().expect("a new pane is written");
        assert_eq!(ledger.changed(), None, "and not again");
        delivered.store(true, Ordering::Relaxed);
        let second = ledger.changed().expect("delivery, set by the sending thread, is written too");
        assert_ne!(first, second);

        let back = Ledger::decode(&second);
        assert!(back[&7].is_for(8, 1_794_373_200));
        assert!(back[&7].landed_on(8));
        let mut back = back;
        assert_eq!(back.changed(), None, "what was read is what is on disk");
        assert!(Ledger::decode("garbage\n7\tx\n").is_empty());
    }

    #[test]
    fn a_session_whose_work_was_ended_is_told_so_after_the_move() {
        let said = after_ended_work("carry on");
        assert!(said.starts_with("carry on"));
        assert!(said.contains("后台任务"));
    }
}
