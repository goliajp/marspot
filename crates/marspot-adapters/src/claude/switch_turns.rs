//! A session's own replies as the switch-cost measure reads them: one
//! `Turn` per reply of the main conversation, oldest first.
//!
//! A reply is written as several records that repeat its usage, growing;
//! the largest count of each kind is the reply's.  Side-chain records
//! (agents running inside the session) are not the conversation's own
//! prompt and are left out.

use marspot_agent::sched::switch_cost::Turn;
use marspot_json::Value;

use super::text::record_time;

fn count(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::i64).unwrap_or(0).max(0) as u64
}

fn turn_of(at: i64, usage: &Value) -> Turn {
    let written = count(usage, "cache_creation_input_tokens");
    let (write_5m, write_1h) = match usage.get("cache_creation") {
        Some(c) => (count(c, "ephemeral_5m_input_tokens"), count(c, "ephemeral_1h_input_tokens")),
        None => (written, 0),
    };
    Turn { at, input: count(usage, "input_tokens"), write_5m, write_1h, read: count(usage, "cache_read_input_tokens"), output: count(usage, "output_tokens") }
}

fn grow(t: &mut Turn, o: &Turn) {
    t.input = t.input.max(o.input);
    t.write_5m = t.write_5m.max(o.write_5m);
    t.write_1h = t.write_1h.max(o.write_1h);
    t.read = t.read.max(o.read);
    t.output = t.output.max(o.output);
}

/// The main conversation's replies in `text`, a whole transcript.
pub fn turns(text: &str) -> Vec<Turn> {
    let mut out: Vec<(String, Turn)> = Vec::new();
    for line in text.lines() {
        if !line.contains(r#""usage""#) || line.contains(r#""isSidechain":true"#) {
            continue;
        }
        let Ok(v) = marspot_json::parse(line) else { continue };
        if v.str_at("type") != Some("assistant") {
            continue;
        }
        let (Some(msg), Some(at)) = (v.get("message"), record_time(&v)) else { continue };
        let Some(usage) = msg.get("usage") else { continue };
        let t = turn_of(at as i64, usage);
        let id = msg.str_at("id").unwrap_or("");
        match out.iter_mut().rev().take(8).find(|(i, _)| !id.is_empty() && i == id) {
            Some((_, was)) => grow(was, &t),
            None => out.push((id.to_string(), t)),
        }
    }
    out.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
#[path = "switch_turns_tests.rs"]
mod tests;
