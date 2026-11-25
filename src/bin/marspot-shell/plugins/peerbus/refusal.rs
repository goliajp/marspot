//! A completed turn the daemon says was refused for usage.
//!
//! The app-server's turn error names its kind in `codexErrorInfo`; the
//! rollout writes the same kind as `usage_limit_exceeded`.  A kind
//! with details of its own is an object keyed by its name, so both the
//! bare name and the keyed form are read.

use marspot_peer::json::Value;

const KINDS: [&str; 2] = ["usageLimitExceeded", "usage_limit_exceeded"];

/// `turn` (a `turn/completed`'s `params.turn`) ended refused for usage.
pub(super) fn usage_refused(turn: &Value) -> bool {
    match turn.at(&["error", "codexErrorInfo"]) {
        Some(Value::Str(s)) => KINDS.contains(&s.as_str()),
        Some(Value::Obj(kv)) => kv.iter().any(|(k, _)| KINDS.contains(&k.as_str())),
        _ => false,
    }
}

/// How a thread's last turns ended, stamped when the daemon said so.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnEnds {
    /// The last turn refused for usage: when, and its id.
    pub refused: Option<(i64, Option<String>)>,
    /// The last turn that ended without that.
    pub done_at: Option<i64>,
}

impl TurnEnds {
    /// A `turn/completed` was heard at `now`; `turn` is its `params.turn`.
    pub(super) fn completed(&mut self, turn: Option<&Value>, now: i64) {
        match turn {
            Some(t) if usage_refused(t) => self.refused = Some((now, t.str_at("id").map(str::to_string))),
            _ => self.done_at = Some(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json;

    #[test]
    fn peerbus_turn_refused_for_usage_in_either_form() {
        let turn = |info: &str| json::parse(&format!(r#"{{"id":"t","status":"failed","error":{{"message":"You've hit your usage limit.","codexErrorInfo":{info}}}}}"#)).unwrap();
        assert!(usage_refused(&turn(r#""usageLimitExceeded""#)));
        assert!(usage_refused(&turn(r#"{"usageLimitExceeded":{"limitId":"codex","resetsAt":1796051701}}"#)));
        assert!(!usage_refused(&turn(r#""contextWindowExceeded""#)));
        assert!(!usage_refused(&json::parse(r#"{"id":"t","status":"completed","error":null}"#).unwrap()));
    }

    /// A good turn after a refusal is heard as one, so a pane without a
    /// rollout does not stay refused.
    #[test]
    fn peerbus_turn_ends_record_refused_and_done() {
        let mut e = TurnEnds::default();
        let refused = json::parse(r#"{"id":"t1","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}"#).unwrap();
        e.completed(Some(&refused), 100);
        assert_eq!(e, TurnEnds { refused: Some((100, Some("t1".into()))), done_at: None });
        let good = json::parse(r#"{"id":"t2","status":"completed","error":null}"#).unwrap();
        e.completed(Some(&good), 160);
        assert_eq!(e, TurnEnds { refused: Some((100, Some("t1".into()))), done_at: Some(160) });
    }
}
