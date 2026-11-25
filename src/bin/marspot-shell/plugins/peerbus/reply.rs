//! Sending a Codex session's answer back to the session that asked.
//!
//! A message from another session reaches Codex as input to its
//! thread, and Codex answers it as it answers anything: in its own
//! pane. Its `SendMessage` tool would carry the answer back, and it
//! does not reach for it -- on 2026-11-21 a Codex pane answered a
//! Claude session's question in full, twenty minutes passed, and the
//! asker never heard. So once the turn that took the message ends, its
//! final answer goes back to the sender, unless Codex already sent one
//! there itself.
//!
//! A relayed answer is itself a message, and the session it reaches
//! answers it in a turn of its own. Relaying that answer too would
//! send two Codex sessions back and forth until the rate limit stops
//! them, so a turn a relayed answer started owes nobody an answer.

use std::collections::HashMap;

use marspot_peer::json::Value;

/// Messages handed over whose turn has not shown up yet. A message
/// queued behind a long turn waits here for that turn to end.
const WAITING_MAX: usize = 16;
/// Turns owing answers at once; one thread runs one turn at a time.
const TURNS_MAX: usize = 8;

/// Who asked, and how they may be named back.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Asker {
    pub(super) reply_to: String,
    pub(super) name: Option<String>,
}

impl Asker {
    /// Is `to`, as Codex wrote it in a `SendMessage`, this asker?
    fn is(&self, to: &str) -> bool {
        to == self.reply_to || self.name.as_deref().is_some_and(|n| to == n || to.starts_with(&format!("{n} ")))
    }
}

/// Who asked, for a message whose content is `content`: nobody when
/// the message is an answer this module relayed.
pub(super) fn asker(content: &str, reply_to: String) -> Option<Asker> {
    let env = marspot_peer::envelope::parse(content);
    if env.as_ref().is_some_and(|e| e.from_plugin.as_deref() == Some(RELAY_PLUGIN) || e.body.trim_start().starts_with(RELAYED_UNMARKED)) {
        return None;
    }
    Some(Asker { reply_to, name: env.and_then(|e| e.from_name) })
}

#[derive(Default)]
struct Turn {
    askers: Vec<Asker>,
    /// The last thing the turn said, and whether it was its final answer.
    answer: Option<(String, bool)>,
    /// Where Codex sent messages itself.
    sent_to: Vec<String>,
}

/// One Codex thread's unanswered askers.
#[derive(Default)]
pub(super) struct Replies {
    waiting: Vec<(String, Asker)>,
    turns: HashMap<String, Turn>,
    /// How the thread's turns ended, for the thread view.
    pub(super) ends: super::refusal::TurnEnds,
}

impl Replies {
    /// A message was queued with this client id.
    pub(super) fn queued(&mut self, client_id: String, asker: Asker) {
        if self.waiting.len() == WAITING_MAX {
            self.waiting.remove(0);
        }
        self.waiting.push((client_id, asker));
    }

    /// A message was put into the running turn.
    pub(super) fn steered(&mut self, turn: &str, asker: Asker) {
        self.turn(turn).askers.push(asker);
    }

    fn turn(&mut self, id: &str) -> &mut Turn {
        if !self.turns.contains_key(id) && self.turns.len() == TURNS_MAX {
            // a turn whose end was never seen; it owes nothing any more
            let stale = self.turns.keys().next().cloned().expect("full");
            self.turns.remove(&stale);
        }
        self.turns.entry(id.to_string()).or_default()
    }

    /// An item of `turn` completed.
    pub(super) fn item(&mut self, turn: &str, item: &Value) {
        match item.str_at("type") {
            Some("userMessage") => {
                let Some(client) = item.str_at("clientId") else { return };
                let Some(i) = self.waiting.iter().position(|(c, _)| c == client) else { return };
                let (_, asker) = self.waiting.remove(i);
                self.turn(turn).askers.push(asker);
            }
            Some("agentMessage") => {
                let Some(t) = self.turns.get_mut(turn) else { return };
                let Some(text) = item.str_at("text").filter(|s| !s.trim().is_empty()) else { return };
                let fin = item.str_at("phase") == Some("final_answer");
                // a final answer is not replaced by commentary after it
                if !t.answer.as_ref().is_some_and(|(_, f)| *f && !fin) {
                    t.answer = Some((text.to_string(), fin));
                }
            }
            Some("mcpToolCall") => {
                let Some(t) = self.turns.get_mut(turn) else { return };
                if item.str_at("server") == Some("marspot") && item.str_at("tool") == Some("SendMessage") {
                    if let Some(to) = item.get("arguments").and_then(|a| a.str_at("to")) {
                        t.sent_to.push(to.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    /// The turn ended: the answer each asker it owes is still owed.
    pub(super) fn completed(&mut self, turn: &str) -> Vec<(Asker, String)> {
        let Some(t) = self.turns.remove(turn) else { return Vec::new() };
        let Some((answer, _)) = t.answer else { return Vec::new() };
        t.askers
            .into_iter()
            .filter(|a| !t.sent_to.iter().any(|to| a.is(to)))
            .map(|a| (a, answer.clone()))
            .collect()
    }
}

/// The `from-plugin` of a relayed answer: what tells the receiver not
/// to relay its own answer to it back.
pub(super) const RELAY_PLUGIN: &str = "marspot-relay";

/// Kept equal to `RELAYED_UNMARKED` while v0.6.2 is still in use: that
/// build recognises nothing else.
const RELAYED: &str = "(Relayed by marspot: this Codex session's answer to your message, from its turn.)";

/// How builds up to v0.6.2 and the next few began a relayed answer,
/// which they did not mark otherwise.  Frozen, whatever `RELAYED` says
/// later: a build from before the mark still relays everything, and a
/// relayed answer from it is known only by this.
const RELAYED_UNMARKED: &str = "(Relayed by marspot: this Codex session's answer to your message, from its turn.)";

/// What the asker is sent: the answer, said to be relayed.
fn body(answer: &str) -> String {
    format!("{RELAYED}\n\n{answer}")
}

/// The whole relayed answer, as this session.
pub(super) fn envelope(from: &marspot_peer::envelope::Header, answer: &str) -> String {
    marspot_peer::envelope::encode_from_plugin(from, Some(RELAY_PLUGIN), &body(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json::parse;

    fn asker() -> Asker {
        Asker { reply_to: "uds:/tmp/cc-socks/16618.sock".into(), name: Some("GoliaQIP 与 QIP MobileApp".into()) }
    }

    fn item(json: &str) -> Value {
        parse(json).unwrap()
    }

    fn says(phase: &str, text: &str) -> Value {
        item(&format!(r#"{{"type":"agentMessage","id":"m","text":"{text}","phase":"{phase}"}}"#))
    }

    /// 2026-11-21 09:48: the message queued, the turn ran, the answer
    /// stayed in the pane.
    #[test]
    fn the_final_answer_of_the_turn_a_message_started_goes_back() {
        let mut r = Replies::default();
        r.queued("c1".into(), asker());
        r.item("t1", &item(r#"{"type":"userMessage","id":"u","clientId":"c1","content":[]}"#));
        r.item("t1", &says("commentary", "checking"));
        r.item("t1", &says("final_answer", "Safari, by AppleScript"));
        r.item("t1", &says("commentary", "done"));
        let back = r.completed("t1");
        assert_eq!(back, vec![(asker(), "Safari, by AppleScript".to_string())]);
        assert!(r.completed("t1").is_empty(), "once");
    }

    #[test]
    fn a_turn_nobody_asked_for_or_one_that_said_nothing_sends_nothing() {
        let mut r = Replies::default();
        r.item("t0", &says("final_answer", "for the user"));
        assert!(r.completed("t0").is_empty());
        r.steered("t2", asker());
        assert!(r.completed("t2").is_empty(), "no answer to send");
    }

    #[test]
    fn an_answer_codex_sent_itself_is_not_sent_again() {
        for to in ["GoliaQIP 与 QIP MobileApp", "uds:/tmp/cc-socks/16618.sock", "GoliaQIP 与 QIP MobileApp [3c0340]"] {
            let mut r = Replies::default();
            r.steered("t3", asker());
            r.item("t3", &item(&format!(r#"{{"type":"mcpToolCall","id":"x","server":"marspot","tool":"SendMessage","arguments":{{"to":"{to}","message":"hi"}}}}"#)));
            r.item("t3", &says("final_answer", "sent"));
            assert!(r.completed("t3").is_empty(), "{to}");
        }
        let mut r = Replies::default();
        r.steered("t4", asker());
        r.item("t4", &item(r#"{"type":"mcpToolCall","id":"x","server":"marspot","tool":"SendMessage","arguments":{"to":"someone-else"}}"#));
        r.item("t4", &says("final_answer", "a"));
        assert_eq!(r.completed("t4").len(), 1, "a message to someone else is not the answer");
    }

    /// 2026-11-22: two Codex panes relayed "ok" to each other until the
    /// rate limit refused them.
    #[test]
    fn the_answer_to_a_relayed_answer_is_not_relayed_back() {
        use marspot_peer::envelope::{encode, Header};
        let a = "uds:/tmp/cc-socks/a.sock";
        let b = "uds:/tmp/cc-socks/b.sock";
        let question = encode(&Header { from: a.into(), from_name: Some("proj#1001".into()), from_mode: None }, "what is 2+2");
        let mut at_b = Replies::default();
        at_b.steered("tb", asker_of(&question, a));
        at_b.item("tb", &says("final_answer", "4"));
        let back = at_b.completed("tb");
        assert_eq!(back.len(), 1, "A's question is answered");

        let relayed = envelope(&Header { from: b.into(), from_name: Some("proj#1002".into()), from_mode: None }, &back[0].1);
        assert_eq!(super::asker(&relayed, b.into()), None, "the relayed answer names no asker");
        let mut at_a = Replies::default();
        if let Some(who) = super::asker(&relayed, b.into()) {
            at_a.steered("ta", who);
        }
        at_a.item("ta", &says("final_answer", "ok"));
        assert!(at_a.completed("ta").is_empty(), "A's answer to the relay does not go back to B");
    }

    /// The mark is in the header: a relayed answer is known whatever
    /// its body says, and a body that merely starts like one in a
    /// header without the mark is known only by the frozen old wording.
    #[test]
    fn a_relayed_answer_is_known_by_its_mark_not_its_wording() {
        use marspot_peer::envelope::{encode, encode_from_plugin, Header};
        let b = "uds:/tmp/cc-socks/b.sock";
        let h = Header { from: b.into(), from_name: Some("proj#1002".into()), from_mode: None };
        for reworded in ["(Answer forwarded by marspot.)\n\nok", "ok", "マーズポットが転送した回答\n\nok"] {
            let relayed = encode_from_plugin(&h, Some(RELAY_PLUGIN), reworded);
            assert_eq!(super::asker(&relayed, b.into()), None, "{reworded}");
        }
        let plain = encode(&h, "(Answer forwarded by marspot.)\n\nok");
        assert!(super::asker(&plain, b.into()).is_some(), "the wording alone is not the mark");
        let other = encode_from_plugin(&h, Some("someone-else"), "hi");
        assert!(super::asker(&other, b.into()).is_some(), "another plugin's message is a question");
    }

    /// A build from before the mark relays every answer, its relays
    /// unmarked; answering one of those back would never stop.
    #[test]
    fn an_unmarked_relay_from_an_older_build_is_still_known() {
        use marspot_peer::envelope::{encode, Header};
        let h = Header { from: "uds:/tmp/cc-socks/b.sock".into(), from_name: None, from_mode: None };
        let v062 = encode(&h, "(Relayed by marspot: this Codex session's answer to your message, from its turn.)\n\nok");
        assert_eq!(super::asker(&v062, "uds:/tmp/cc-socks/b.sock".into()), None);
    }

    fn asker_of(content: &str, reply_to: &str) -> Asker {
        let who = super::asker(content, reply_to.into()).expect("a plain message has an asker");
        assert_eq!(who.name.as_deref(), Some("proj#1001"));
        who
    }

    #[test]
    fn what_waits_is_bounded() {
        let mut r = Replies::default();
        for i in 0..40 {
            r.queued(format!("c{i}"), asker());
            r.steered(&format!("t{i}"), asker());
        }
        assert_eq!(r.waiting.len(), WAITING_MAX);
        assert_eq!(r.turns.len(), TURNS_MAX);
    }
}
