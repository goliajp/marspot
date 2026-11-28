//! What the executor tells a session, and where it keeps the ladder.
//!
//! Every message goes to the session's own inbox, never into the
//! composer, and asks for no reply.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use marspot_agent::sched::level2::Kind;

use super::super::heads_up;

const TAG: &str = "sched.exec.told";

fn send(sid: u64, claude_pid: i32, profile: u8, body: String) {
    heads_up::send_to(sid, profile, body, Arc::new(AtomicBool::new(false)), TAG, move || heads_up::inbox_of(claude_pid));
}

/// The notice of a planned switch: the five-minute one, or for a session
/// with long background work the drain notice.
pub(super) fn notice(sid: u64, claude_pid: i32, from: u8, to: u8, kind: Kind) {
    send(sid, claude_pid, from, notice_body(from, to, kind));
}

pub(super) fn notice_body(from: u8, to: u8, kind: Kind) -> String {
    let head = format!("marspot 提醒：为了用好各账号的额度，这个会话会按计划从账号 P{from} 换到 P{to}，对话会原样接上。");
    let what = if kind == Kind::Drain {
        "现在在运行的后台任务和后台子代理结束以后，请先不要启动新的；在没有后台任务的回合结束时 marspot 会切换。"
    } else {
        "大约 5 分钟后，在回合结束时切换。请照常继续手头的任务，不用停下等待。切换会结束还在运行的后台任务，正在运行的和新启动的后台任务请记下命令和输出位置。"
    };
    format!("{head}{what}切换完成后 marspot 会发消息让你接着做。这条消息不用回复。")
}

/// A noticed switch is called off: carry on as before.
pub(super) fn carry_on(sid: u64, claude_pid: i32, profile: u8) {
    send(sid, claude_pid, profile, carry_on_body());
}

pub(super) fn carry_on_body() -> String {
    "marspot：之前提醒的账号切换不需要了，请照常继续，后台任务也可以照常启动。这条消息不用回复。".to_string()
}

/// Said to a session after a planned switch.
pub(in crate::plugins::claudecode) fn after_switch(from: u8, to: u8) -> String {
    format!(
        "marspot 按计划把这个会话从账号 P{from} 换到了 P{to}，对话已经接上。请接着做切换前在做的事；如果你在等用户回答，或者任务已经做完，就继续等用户。\
         换账号时还在运行的后台任务和后台子代理已经被结束了，需要的话请检查并重新启动"
    )
}

pub(super) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

impl super::Exec {
    /// Keeps the ladder when it changed, and logs its counters when they
    /// changed, at most once per full pass.
    pub(super) fn save(&mut self, full: usize, log: &mut Vec<(&'static str, String)>) {
        let words = self.ladder.words();
        if words != self.said.0 && full > self.said.1 {
            log.push(("sched.gate", words.clone()));
            self.said = (words, full);
        }
        let text = self.ladder.encode();
        if text == self.saved {
            return;
        }
        if let Err(e) = write_atomic(&self.path, &text) {
            log.push(("sched.gate.save_failed", format!("{e}")));
        }
        self.saved = text;
    }
}
