//! Claude Code's session record: one JSON object per line, appended.

use marspot_agent::types::{FoldSink, RecordFold};

mod tasks;
pub mod text;
mod usage;

/// Fold the bytes appended to one record file into `st`, saying each
/// reply, compaction and task change to `out` on the way.
///
/// `appended` starts at `st.at`.  Only whole lines are folded; `st.at`
/// ends just past the last newline, and what follows it is handed over
/// again, longer, by the next call.
pub fn fold(appended: &[u8], st: &mut RecordFold, out: &mut dyn FoldSink) {
    let mut rest = appended;
    if st.mid_record {
        let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
            st.at += rest.len() as u64;
            return;
        };
        st.at += nl as u64 + 1;
        st.mid_record = false;
        rest = &rest[nl + 1..];
    }
    while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
        if let Ok(line) = std::str::from_utf8(&rest[..nl]) {
            record(line, st, out);
        }
        st.at += nl as u64 + 1;
        rest = &rest[nl + 1..];
    }
}

fn record(line: &str, st: &mut RecordFold, out: &mut dyn FoldSink) {
    let kind = text::top_level_str(line, "type");
    // most lines are bookkeeping; the parse is paid only for these
    const WANTED: [&str; 6] = ["\"assistant\"", "\"usage\"", "\"user\"", "turn_duration", "stop_hook_summary", "queue-operation"];
    if !WANTED.iter().any(|k| line.contains(k)) {
        return;
    }
    let v = marspot_json::parse(line).ok();
    usage::take_context(line, kind, v.as_ref(), st);
    usage::take_reply(line, v.as_ref(), st, out);
    tasks::take(line, v.as_ref(), st, out);
}

#[cfg(test)]
mod tests;
