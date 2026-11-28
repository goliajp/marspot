//! The arguments of a request that take more than a lookup: a pane to
//! open, an input to send, a command to run, a wait.

use super::{Args, Input, MAX_WAIT_TEXT, Op, OpenArgs, Wait, bad};
use crate::control::{ErrorCode, MAX_SEND_BYTES, Reject, b64, keys};
use crate::json::Value;

pub(super) fn open(a: &Args) -> Result<Op, Reject> {
    let new_window = a.bool("new_window", false)?;
    let o = OpenArgs {
        cwd: a.str("cwd")?,
        shadow: a.bool("shadow", !new_window)?,
        new_window,
        parent: a.str("parent")?,
        cols: a.size("cols", 120)?,
        rows: a.size("rows", 40)?,
        command: a.str("command")?,
    };
    if new_window && (o.shadow || o.parent.is_some()) {
        return Err(bad("a pane in a new window is not a shadow and has no `parent`"));
    }
    Ok(Op::Open(o))
}

pub(super) fn input(a: &Args) -> Result<Input, Reject> {
    let text = a.str("text")?;
    let keys_v = a.0.get("keys");
    let b = a.str("bytes_b64")?;
    let given = text.is_some() as u8 + keys_v.is_some() as u8 + b.is_some() as u8;
    if given != 1 {
        return Err(bad("give exactly one of `text`, `keys`, `bytes_b64`"));
    }
    let input = if let Some(text) = text {
        Input::Text { text, enter: a.bool("enter", true)? }
    } else if let Some(k) = keys_v {
        let names: Vec<String> = k.arr().iter().filter_map(Value::str).map(String::from).collect();
        if names.is_empty() || names.len() != k.arr().len() {
            return Err(bad("`keys` is a non-empty array of key names"));
        }
        if let Some(unknown) = names.iter().find(|n| keys::bytes(n).is_none()) {
            return Err(bad(format!("unknown key `{unknown}`; known: {}", keys::names().join(" "))));
        }
        Input::Keys(names)
    } else {
        let raw = b.unwrap_or_default();
        Input::Bytes(b64::decode(&raw).ok_or_else(|| bad("`bytes_b64` is not base64"))?)
    };
    if input.len() > MAX_SEND_BYTES {
        return Err(Reject::new(ErrorCode::LimitBytes, format!("{} bytes; one send carries at most 32 KiB", input.len())));
    }
    Ok(input)
}

/// The status line `pane.run` adds is a few dozen bytes more: the
/// command is held to what one send carries.
pub(super) fn run_command(c: Option<String>) -> Result<String, Reject> {
    let c = c.filter(|c| !c.trim().is_empty()).ok_or_else(|| bad("`command` is required"))?;
    if c.len() > MAX_SEND_BYTES {
        return Err(Reject::new(ErrorCode::LimitBytes, format!("{} bytes; one command carries at most 32 KiB", c.len())));
    }
    Ok(c)
}

pub(super) fn wait(v: Option<&Value>) -> Result<Option<Wait>, Reject> {
    let Some(v) = v.filter(|v| !matches!(v, Value::Null)) else { return Ok(None) };
    let on = |k: &str| v.get(k).and_then(Value::bool) == Some(true);
    if let Some(t) = v.get("text") {
        let t = t.str().filter(|t| !t.is_empty()).ok_or_else(|| bad("`wait.text` is a non-empty string"))?;
        if t.len() > MAX_WAIT_TEXT {
            return Err(bad("`wait.text` is at most 256 bytes"));
        }
        return Ok(Some(Wait::Text(t.to_string())));
    }
    if let Some(q) = v.get("quiet_ms") {
        return match q.i64() {
            Some(ms @ 100..=600_000) => Ok(Some(Wait::Quiet(ms as u32))),
            _ => Err(bad("`wait.quiet_ms` is between 100 and 600000")),
        };
    }
    for (k, w) in [("command_end", Wait::CommandEnd), ("prompt", Wait::Prompt), ("exit", Wait::Exit), ("change", Wait::Change)] {
        if on(k) {
            return Ok(Some(w));
        }
    }
    Err(bad("`wait` is one of {text}, {quiet_ms}, {command_end:true}, {prompt:true}, {exit:true}, {change:true}"))
}
