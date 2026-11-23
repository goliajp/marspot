//! Telling a busy session its account is about to run out, while the
//! account can still answer.
//!
//! Once an account refuses, the claude on it cannot send a request,
//! so anything it is told then goes unanswered and the wait before a
//! move buys nothing but a pane standing still. The last of the week
//! is the only time a session can still act on the news, and what it
//! is asked to do is keep working until the switch comes and be ready
//! for it: write down what runs in the background. The message goes to
//! the session's own inbox, never into the composer, so what the
//! person is typing is not touched.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
/// The feed gives a week's renewal to the second, and two reads of one
/// week can differ by that second: 23:59:59 one time, 00:00:00 the
/// next. Weeks renew seven days apart, so renewals within a day of each
/// other are the same week.
const SAME_WEEK: u64 = 86_400;

/// Is this account, for a pane on `model`, one step from refusing it?
pub(in crate::plugins) fn nearly_out(room: &Room, model: Option<&str>) -> bool {
    if room.refused_for(model) {
        return false;
    }
    match room {
        Room::Known { .. } => {
            let (week, five_hours) = room.headroom_for(model);
            week.is_some_and(|w| w <= LAST_OF_WEEK + EPS) || five_hours <= LAST_OF_FIVE_HOURS + EPS
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
        self.profile == profile && self.week.abs_diff(week) < SAME_WEEK
    }
    pub(in crate::plugins) fn landed_on(&self, profile: u8) -> bool {
        self.profile == profile && self.delivered.load(Ordering::Relaxed)
    }
    /// The account it was about came back, at a new five hours or a new
    /// week: the record is dropped, so the next time it nears out the
    /// pane is told again and a later move does not count as told.
    pub(in crate::plugins) fn renewed_on(&self, profile: u8, room: &Room, model: Option<&str>) -> bool {
        self.profile == profile && *room != Room::Unknown && !room.refused_for(model) && !nearly_out(room, model)
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
///
/// It must not read as a reason to wrap up. On 2026-11-22 the earlier
/// wording asked for the step in hand to be brought to a point where it
/// could be interrupted, and for no long background work to be started.
/// All five panes told at 15:00:38 ended their turn within half a
/// minute, saying they could now be interrupted; one had its subagent
/// stop as well. Four sat still until the person typed "carry on", on
/// an account that went on answering for nine more minutes.
///
/// `told_after`: whether the move is followed by a word asking the
/// session to go on, which the session is then promised.
pub(in crate::plugins) fn body(profile: u8, told_after: bool) -> String {
    let after = if told_after { "切换完成后 marspot 会发消息让你接着做。" } else { "" };
    format!(
        "marspot 提醒：这个会话所用的账号（P{profile}）额度只剩不到 1%，随时可能被换到另一个账号上重启。\
         请照常继续手头的任务，不要因为这条提醒停下、收尾或者等待切换，也不要让子代理停下；在切换之前，这个账号还能正常使用。\
         切换会结束还在运行的后台任务和后台子代理，所以请做好随时被打断的准备：正在运行的和以后启动的后台任务，把命令和输出位置记下来，被打断后好接着做。\
         {after}这条消息不用回复。"
    )
}

/// Said after a move to a session that had work running in the
/// background when its claude was taken down.
pub(super) fn after_ended_work(resume_word: &str) -> String {
    format!("{resume_word}。{ENDED_WORK}")
}

const ENDED_WORK: &str = "换账号时还在运行的后台任务和后台子代理已经被结束了，此前因额度用尽而失败的子代理也没有做完，\
                          请检查它们的状态，需要的话重新启动";

/// Said after a move to a session that was told its account was
/// nearly out.
///
/// Such a session was working when it was told, so the usual word's
/// "stay idle if idle" is wrong for it: one that stopped after the
/// warning reads itself as idle and keeps still on the new account.
/// It is no order to go on either: by the time of the move the session
/// may have finished its task or be waiting for the person's answer,
/// and then it keeps waiting.
pub(super) fn after_warned_move(work_ended: bool) -> String {
    let word = "额度提醒里说的账号切换已经完成。如果你是因为额度提醒停下的，或者在等这次切换，现在就接着做切换前在做的任务；\
                如果你在等用户回答，或者任务已经做完，就继续等用户";
    if work_ended { format!("{word}。{ENDED_WORK}") } else { word.to_string() }
}

/// The socket name marspot's own messages say they are from.
pub(in crate::plugins) const SENDER: &str = "marspot-quota.sock";

/// The frame for `inbox`, from marspot.
///
/// marspot acts for the person without asking first, which is what
/// `bypass` names, and the sessions it serves run the same way. The
/// reply address sits beside the inbox and nothing listens on it; the
/// message asks for no reply.
pub(in crate::plugins) fn frame_line(inbox: &str, body: &str) -> String {
    let dir = Path::new(inbox).parent().map(|d| d.display().to_string()).unwrap_or_default();
    let from = format!("uds:{dir}/{SENDER}");
    let header = Header { from: from.clone(), from_name: Some("marspot".into()), from_mode: Some(Mode::Bypass) };
    frame::user_line(&envelope::encode(&header, body), &from, Priority::Next, &marspot_peer::rand::uuid_v4())
}

/// The inbox of the claude with this pid, from the sessions registry.
fn inbox_of(claude_pid: i32) -> Option<String> {
    marspot_peer::peers::live_entries().into_iter().find(|e| e.pid == claude_pid && !e.ours)?.socket
}

/// Off the drawing thread: it reads the registry and writes a socket.
pub(super) fn send(sid: u64, claude_pid: i32, profile: u8, delivered: Arc<AtomicBool>) {
    send_to(sid, profile, body(profile, true), delivered, "claudecode.quota.heads_up", move || inbox_of(claude_pid));
}

/// `body` for a session whose inbox is found by `inbox`.
pub(in crate::plugins) fn send_to(
    sid: u64,
    profile: u8,
    body: String,
    delivered: Arc<AtomicBool>,
    tag: &'static str,
    inbox: impl FnOnce() -> Option<String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = match inbox() {
            None => "the session has no inbox".to_string(),
            Some(inbox) => match marspot_peer::inbox::write(&inbox, &frame_line(&inbox, &body)) {
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
            util_5h: Some(util_5h),
            util_7d: Some(util_7d),
            reset_7d: 1000,
            models: fable.map(|u| vec![("fable".to_string(), Some(u))]).unwrap_or_default(),
            models_measured_at: 0,
            measured_at: 0,
        }
    }

    #[test]
    fn a_told_record_is_renewed_only_when_its_account_has_room_again() {
        let sent = Sent { profile: 2, week: 1000, delivered: Arc::new(AtomicBool::new(true)) };
        assert!(sent.renewed_on(2, &room("allowed", 0.1, 0.5, None), None));
        assert!(!sent.renewed_on(3, &room("allowed", 0.1, 0.5, None), None), "another account");
        assert!(!sent.renewed_on(2, &room("allowed", 0.0, 0.99, None), None), "still nearly out");
        assert!(!sent.renewed_on(2, &room("rejected", 0.0, 1.0, None), None), "out");
        assert!(!sent.renewed_on(2, &Room::Unknown, None), "not read");
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
        let me = std::process::id() as i32;
        let rec = format!(
            "{{\"pid\":{me},\"procStart\":\"{}\",\"messagingSocketPath\":\"{inbox}\"}}",
            marspot_peer::proc::lstart(me).unwrap()
        );
        std::fs::write(dir.join(format!("{me}.json")), rec).unwrap();
        marspot_peer::inbox::write_in(std::slice::from_ref(&dir), &inbox, &frame_line(&inbox, &body(8, true))).unwrap();
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
        assert!(after_warned_move(true).contains("后台任务"));
        assert!(!after_warned_move(false).contains("后台任务"));
    }

    #[test]
    fn the_heads_up_asks_to_keep_working_and_never_to_wrap_up() {
        let said = body(1, true);
        assert!(said.contains("P1"));
        assert!(said.contains("切换完成后"), "claude and kimi panes are told after the move: {said}");
        assert!(!body(1, false).contains("切换完成后"), "a pane is not promised what it is not sent");
        assert!(body(1, false).contains("照常继续"));
        assert!(said.contains("照常继续"));
        assert!(said.contains("随时"), "a switch may come at any moment: {said}");
        assert!(said.contains("被打断"), "be ready to be interrupted: {said}");
        assert!(said.contains("不要因为这条提醒停下"), "{said}");
        // the words that made five sessions stop on 2026-11-22
        for wrap_up in ["做到可以中断的状态", "不要再启动", "停在"] {
            assert!(!said.contains(wrap_up), "{wrap_up:?} in {said}");
        }
    }

    #[test]
    fn a_warned_session_is_never_told_to_stay_idle_after_the_move() {
        for ended in [false, true] {
            let said = after_warned_move(ended);
            assert!(said.contains("因为额度提醒停下"), "{said}");
            assert!(said.contains("接着做"), "{said}");
            assert!(!said.contains("idle"), "{said}");
            assert!(!said.contains("保持"), "{said}");
        }
    }

    /// A session that finished its task, or asked the person something,
    /// is not pushed on by the move.
    #[test]
    fn a_warned_session_waiting_for_the_user_keeps_waiting_after_the_move() {
        for ended in [false, true] {
            let said = after_warned_move(ended);
            assert!(said.contains("在等用户回答"), "{said}");
            assert!(said.contains("任务已经做完"), "{said}");
            assert!(said.contains("继续等用户"), "{said}");
            assert!(!said.contains("不用等"), "no unconditional go-ahead: {said}");
        }
    }
}
