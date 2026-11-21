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

/// What the asker is sent: the answer, said to be relayed.
pub(super) fn body(answer: &str) -> String {
    format!("(Relayed by marspot: this Codex session's answer to your message, from its turn.)\n\n{answer}")
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
