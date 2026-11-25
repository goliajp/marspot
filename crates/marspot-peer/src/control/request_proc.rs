//! The `proc.*` requests: end processes that belong to the caller's own
//! pane, each named by pid and start time.

use super::{Args, bad};
use crate::control::{ErrorCode, Reject};
use crate::json::Value;

/// The most processes one `proc.stop` names.
pub const MAX_IDS: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct ProcStopArgs {
    /// `pid@start_us`, as given; each is checked on its own.
    pub ids: Vec<String>,
    /// The process's descendants go with it.
    pub tree: bool,
    /// What is still running five seconds after SIGTERM gets SIGKILL.
    pub force: bool,
}

/// `pid@start_us`: the pid and the start time in microseconds since the
/// epoch.  None when it is not that shape.
pub fn parse_id(s: &str) -> Option<(i32, u64)> {
    let (pid, start) = s.trim().split_once('@')?;
    let pid: i32 = pid.parse().ok()?;
    let start: u64 = start.parse().ok()?;
    (pid > 0 && start > 0).then_some((pid, start))
}

pub(super) fn stop(a: &Args) -> Result<ProcStopArgs, Reject> {
    let ids: Vec<String> = match a.0.get("ids") {
        Some(Value::Arr(v)) => v.iter().map(|x| x.str().map(String::from).ok_or_else(|| bad("`ids` holds strings: pid@start_us"))).collect::<Result<_, _>>()?,
        Some(Value::Str(s)) => s.split([',', ' ']).filter(|x| !x.is_empty()).map(String::from).collect(),
        _ => return Err(bad("`ids` is required: a list of pid@start_us")),
    };
    if ids.is_empty() {
        return Err(bad("`ids` is empty"));
    }
    if ids.len() > MAX_IDS {
        return Err(Reject::new(ErrorCode::LimitCount, format!("{} ids; one request names at most {MAX_IDS}", ids.len())));
    }
    Ok(ProcStopArgs { ids, tree: a.bool("tree", true)?, force: a.bool("force", false)? })
}

pub(super) fn stop_json(s: &ProcStopArgs) -> Vec<(String, Value)> {
    vec![
        ("ids".into(), Value::Arr(s.ids.iter().map(|i| i.as_str().into()).collect())),
        ("tree".into(), s.tree.into()),
        ("force".into(), s.force.into()),
    ]
}

#[cfg(test)]
mod pane_control_proc_tests {
    use super::super::{Op, parse};
    use super::*;

    #[test]
    fn pane_control_proc_stop_reads_and_writes_back() {
        let r = parse(r#"{"op":"proc.stop","args":{"ids":["12@1795584627123456","9@1"],"force":true}}"#).unwrap();
        let Op::ProcStop(a) = &r.op else { panic!("{r:?}") };
        assert_eq!((a.ids.len(), a.tree, a.force), (2, true, true));
        assert_eq!(parse(&r.to_json()).unwrap(), r);
        assert!(!r.op.is_read_only());
        assert_eq!(r.op.name(), "proc.stop");
    }

    #[test]
    fn pane_control_proc_stop_refuses_a_request_without_ids() {
        for line in [
            r#"{"op":"proc.stop","args":{}}"#,
            r#"{"op":"proc.stop","args":{"ids":[]}}"#,
            r#"{"op":"proc.stop","args":{"ids":[12]}}"#,
            r#"{"op":"proc.stop","args":{"ids":["1@2"],"tree":"yes"}}"#,
        ] {
            assert_eq!(parse(line).unwrap_err().code, ErrorCode::BadRequest, "{line}");
        }
        let many: Vec<String> = (0..=MAX_IDS).map(|i| format!("\"{}@1\"", i + 2)).collect();
        let line = format!(r#"{{"op":"proc.stop","args":{{"ids":[{}]}}}}"#, many.join(","));
        assert_eq!(parse(&line).unwrap_err().code, ErrorCode::LimitCount);
    }

    #[test]
    fn pane_control_proc_ids_are_pid_at_start() {
        assert_eq!(parse_id("36160@1795584627123456"), Some((36160, 1_795_584_627_123_456)));
        for bad in ["36160", "@5", "0@5", "-3@5", "5@0", "a@b", "5@-1", "5@1@2"] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }
}
