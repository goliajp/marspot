//! Token counts out of Claude's assistant records: the context of the
//! last reply, and each reply's counts for what it spent.

use marspot_agent::types::{FoldSink, ModelId, RecordFold, Reply, ShortId, Tokens};
use marspot_json::Value;

use super::text::{names_model, record_time};

fn count(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::i64).unwrap_or(0).max(0) as u64
}

/// A reply's `message.usage` by kind.  Cache writes not split by
/// lifetime count as five-minute ones.
fn tokens_of(usage: &Value) -> Tokens {
    let written = count(usage, "cache_creation_input_tokens");
    let (cache_5m, cache_1h) = match usage.get("cache_creation") {
        Some(c) => (count(c, "ephemeral_5m_input_tokens"), count(c, "ephemeral_1h_input_tokens")),
        None => (written, 0),
    };
    Tokens {
        input: count(usage, "input_tokens"),
        cache_5m,
        cache_1h,
        cache_read: count(usage, "cache_read_input_tokens"),
        output: count(usage, "output_tokens"),
    }
}

/// What one `usage` object counts: what was sent and what was said.
/// None without an input count.
fn sent_and_said(usage: &Value) -> Option<(u64, u64)> {
    let n = |k: &str| usage.get(k).and_then(Value::i64).and_then(|n| u64::try_from(n).ok());
    let sent = [n("input_tokens")?, n("cache_creation_input_tokens").unwrap_or(0), n("cache_read_input_tokens").unwrap_or(0)];
    Some((sent.into_iter().fold(0, u64::saturating_add), n("output_tokens").unwrap_or(0)))
}

/// The context a reply's record gives: what was sent plus what was
/// said.  A record whose usage says nothing was sent says it by its
/// last own iteration (not an advisor's, not a compaction's).
fn context_of(v: &Value) -> Option<u64> {
    let usage = v.at(&["message", "usage"])?;
    let (mut sent, mut said) = sent_and_said(usage)?;
    if sent == 0 {
        let own = |i: &&Value| !i.str_at("type").is_some_and(|t| t.contains("advisor") || t.contains("compaction"));
        (sent, said) = usage.get("iterations")?.arr().iter().rev().find(own).and_then(sent_and_said)?;
    }
    Some(sent.saturating_add(said)).filter(|n| *n > 0)
}

/// The main conversation's context, from an assistant record not of a
/// side chain: the newest record with a context sets it and its model;
/// a newer record that names a model but gives no context moves only
/// the model.
pub fn take_context(line: &str, kind: Option<&str>, v: Option<&Value>, st: &mut RecordFold) {
    if kind != Some("assistant") || line.contains(r#""isSidechain":true"#) {
        return;
    }
    if line.contains(r#""type":"tool_use""#) {
        st.called_tool = Some(true);
    }
    let Some(v) = v else { return };
    let model = v.at(&["message", "model"]).and_then(Value::str).filter(|m| names_model(m)).map(ModelId::new);
    match context_of(v) {
        Some(n) => (st.ctx_tokens, st.model) = (Some(n), model),
        None if model.is_some() => st.model = model,
        None => {}
    }
}

/// One assistant record that carries counts: what it adds to `spent`,
/// and a `used` fact when it has a time.
pub fn take_reply(line: &str, v: Option<&Value>, st: &mut RecordFold, out: &mut dyn FoldSink) {
    if !line.contains(r#""usage""#) {
        return;
    }
    let Some(v) = v.filter(|v| v.str_at("type") == Some("assistant")) else { return };
    let Some(msg) = v.get("message") else { return };
    let Some(usage) = msg.get("usage") else { return };
    let whole = tokens_of(usage);
    let id = msg.str_at("id").unwrap_or("");
    let named = msg.str_at("model").filter(|m| !m.starts_with('<') && !m.contains(char::is_whitespace));
    // a record that names a model is counted against its reply even
    // without a time; one without a time says nothing further
    let grew = match named {
        Some(_) if !id.is_empty() => st.replies.add(id, whole),
        _ => whole,
    };
    let Some(at) = record_time(v) else { return };
    let repeat = !id.is_empty() && st.last_reply.is(id);
    st.last_reply = ShortId::new(id);
    if named.is_some() {
        st.spent.add(&grew);
    }
    let r = Reply { at, model: named.map(ModelId::new), whole, grew, cache_written: count(usage, "cache_creation_input_tokens"), repeat };
    out.used(&r);
}
