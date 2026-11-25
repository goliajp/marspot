//! How large a session's context is, as its record says.

use marspot_peer::json::Value;

use super::session::typed;
use crate::plugins::helpers::used::Used;

/// What `text`, the end of a record, says; `whole`: it is all of it.
pub(super) fn used_in(text: &str, whole: bool) -> Used {
    let last = text.lines().rev().find_map(|l| typed(l, "usage.record"));
    let sum = last.as_ref().and_then(|v| v.get("usage")).map(|u| {
        let n = |k: &str| u.get(k).and_then(Value::i64).and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
        ["inputOther", "output", "inputCacheRead", "inputCacheCreation"].into_iter().map(n).fold(0, u64::saturating_add)
    });
    let called = |l: &str| l.starts_with(r#"{"type":"context.append_loop_event""#) && l.contains(r#""event":{"type":"tool.call""#);
    let had_tool = if text.lines().any(called) { Some(true) } else { whole.then_some(false) };
    Used { tokens: sum.filter(|n| *n > 0), had_tool, model: None }
}

#[cfg(test)]
mod tests {
    use super::super::session::fixture::{CANCELLED_THEN_COMPLETED, TOOL_CALL};
    use super::super::session::{Asker, SessionIndex};
    use super::*;
    use crate::plugins::pidtree::ProcId;

    #[test]
    fn kimi_context_is_read_for_helpers_alone() {
        use super::super::session::fixture::{ID, session};
        let home = std::env::temp_dir().join(format!("marspot-kimi-context-{}", std::process::id()));
        session(&home, ID, TOOL_CALL, Some(4242));
        let proc = ProcId { pid: 4242, start_us: 1 };
        let mut index = SessionIndex::default();
        let mut look = |helper| {
            let started = std::time::SystemTime::now();
            index.look(&Asker { proc, started, home: &home, cwd: None, lone: false, helper }).expect("its session");
            index.used_of(proc)
        };
        assert_eq!(look(false), Some(Used::default()), "nobody is shown it");
        // it becomes a helper with nothing written since
        let read = Some(Used { tokens: Some(13), had_tool: Some(true), model: None });
        assert_eq!(look(true), read);
        assert_eq!(look(false), read);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn kimi_context_is_the_last_usage_summed() {
        let real = r#"{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":480,"output":805,"inputCacheRead":265216,"inputCacheCreation":0},"usageScope":"turn","time":1784263621528}"#;
        let first = real.replace("480", "1");
        assert_eq!(used_in(&format!("{first}\n{real}\n"), false), Used { tokens: Some(266_501), had_tool: None, model: None });
        assert_eq!(used_in(TOOL_CALL, true), Used { tokens: Some(13), had_tool: Some(true), model: None });
        assert_eq!(used_in(CANCELLED_THEN_COMPLETED, true).had_tool, Some(false));
        let most = real.replace("480", "9223372036854775807").replace("805", "9223372036854775807");
        assert_eq!(used_in(&most, false).tokens, Some(u64::MAX));
        assert_eq!(used_in("", false), Used::default());
    }
}
