//! `pane.send` and `pane.run`: a `PaneType` the session writes itself
//! (paste, wait for the echo, Enter), refused while a multi-step script
//! holds the pane, while 8 inputs are in flight to it, or while the
//! user is typing there.

use std::time::{Duration, Instant};

use marspot::shell_proto::{CAP_TYPE_AND_WATCH, MsgType, PaneType, PaneTyped, TypedResult, WatchBody, WatchKind, WatchReason, encode_pane_type};
use marspot_peer::control::{ErrorCode, Input, Reject, RunArgs, SendArgs, keys};
use marspot_peer::json::{Value, obj};

use super::Reply;
use super::caller::CallerKey;
use super::conn::{ANSWER_WITHIN, Asked, Conn, done_reject};
use super::dispatch::{Ctx, audit, target, typing_reject, user_typing};
use super::ops_read::screen_of;

/// Inputs in flight to one pane at most.
pub(super) const MAX_IN_FLIGHT: u8 = 8;

/// The user typed in `sid` a moment ago and `force` was not given.
pub(super) fn gate_typing(conn: &Conn, sid: u64, force: bool) -> Result<(), Reject> {
    if !force && user_typing(conn, sid) { Err(typing_reject()) } else { Ok(()) }
}

fn typed_reject(t: &PaneTyped) -> Option<Reject> {
    let r = match t.result {
        TypedResult::Written => return None,
        TypedResult::PtyQueueFull => Reject::new(ErrorCode::PtyQueueFull, "the pane's write queue is full; nothing was written"),
        TypedResult::QueueFull => Reject::new(ErrorCode::PaneBusy, "the pane has 8 inputs queued"),
        TypedResult::NotReading => Reject::new(
            ErrorCode::PaneNotReading,
            format!("the program in front did not read its input for 2 s; {} bytes went in, Enter was not sent", t.written),
        ),
        TypedResult::NotAtShell => Reject::new(ErrorCode::NotAtShell, "the pane's own shell is not in front"),
        TypedResult::Unknown(b) => Reject::new(ErrorCode::Internal, format!("input result {b}")),
    };
    Some(r)
}

/// How long after a run's command end the next input to that pane
/// first waits for the shell's prompt, and how long it waits at most.
pub(super) const PROMPT_WITHIN: Duration = Duration::from_secs(2);

/// Before input to `sid` that follows a run's command end there within
/// `within`: wait, at most `within`, until the shell is in front and has
/// been quiet for 150 ms.  Input that lands while the shell still draws
/// its prompt (a slow precmd, a git prompt, a loaded machine) is read as
/// lines typed ahead and comes out mangled.  A prompt already drawn
/// answers at once, one round trip.  Past `within` the input goes
/// straight in.
pub(super) fn prompt_first(conn: &mut Conn, caller: CallerKey, sid: u64, within: Duration) -> Result<(), Reject> {
    let Some((at, pos)) = conn.hub.links.take_end(sid) else { return Ok(()) };
    if at.elapsed() >= within {
        return Ok(());
    }
    let body = WatchBody { watch_id: 0, kind: WatchKind::Prompt, since_pos: 0, quiet_ms: 0, text: String::new() };
    // counted from just before the end: the batch that held the end may
    // have drawn the whole prompt, and then no output follows it
    // only a help to the input after it: a wait that cannot be had (the
    // caller's quota is full, say) lets the input through
    let _ = conn.wait(sid, caller, body, Some(pos.saturating_sub(1)), Instant::now() + within);
    Ok(())
}

/// Send `t` and wait for what became of it, counted as in flight.
fn type_into(conn: &mut Conn, sid: u64, t: PaneType) -> Result<PaneTyped, Reject> {
    if !conn.hub.links.begin_input(sid, MAX_IN_FLIGHT) {
        return Err(Reject::new(ErrorCode::PaneBusy, "8 inputs are already on their way to this pane"));
    }
    let asked = conn.ask(sid, MsgType::PaneType, |req_id| encode_pane_type(&PaneType { req_id, ..t.clone() }), true, ANSWER_WITHIN);
    conn.hub.links.end_input(sid);
    match asked {
        Ok(Reply::Typed(t)) => typed_reject(&t).map_or(Ok(t), Err),
        Ok(Reply::Done(d)) => Err(done_reject(d.code, &d.text)),
        Ok(_) => Err(Reject::new(ErrorCode::Internal, "an answer of the wrong kind")),
        Err(Asked::Refused(r)) => Err(r),
        Err(Asked::NoAnswer) => Err(Reject::new(ErrorCode::Unknown, "no answer from the pane; check with pane.read before sending again")),
    }
}

fn admit_bytes(conn: &Conn, ctx: &Ctx, sid: u64, n: usize) -> Result<(), Reject> {
    let now = conn.hub.now_ms();
    conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner()).admit_bytes(&ctx.cfg, sid, n, now)
}

pub(super) fn send(conn: &mut Conn, ctx: &Ctx, a: &SendArgs) -> Result<Value, Reject> {
    let sid = target(ctx, &a.target)?;
    gate_typing(conn, sid, a.force)?;
    conn.need_caps(CAP_TYPE_AND_WATCH)?;
    let (bytes, paste, enter) = match &a.input {
        Input::Text { text, enter } => (text.as_bytes().to_vec(), true, *enter),
        Input::Keys(names) => (names.iter().filter_map(|n| keys::bytes(n)).flatten().copied().collect(), false, false),
        Input::Bytes(b) => (b.clone(), false, false),
    };
    admit_bytes(conn, ctx, sid, bytes.len())?;
    let t = PaneType { req_id: 0, sid, paste, enter, only_at_shell: false, bytes: bytes.clone(), watch: None };
    let out = prompt_first(conn, ctx.caller, sid, PROMPT_WITHIN).and_then(|()| type_into(conn, sid, t));
    let (echo_off, result) = match &out {
        Ok(t) => (t.echo_off, "written".to_string()),
        Err(r) => (false, r.code.as_str().to_string()),
    };
    audit(conn, ctx, "pane.send", sid, Some(bytes), echo_off, &result);
    let t = out?;
    Ok(obj([
        ("pane", super::names::full_name(sid, &ctx.obs).into()),
        ("id", (sid as i64).into()),
        ("pos", (t.pos as i64).into()),
        ("echo_off", t.echo_off.into()),
    ]))
}

/// The two lines `pane.run` pastes: the command, then a line printing
/// the status it left as a command-end mark.
pub(super) fn run_text(command: &str, fish: bool) -> String {
    let status = if fish { "$status" } else { "$?" };
    format!("{}\nprintf '\\033]133;D;%s\\007' {status}", command.trim_end_matches('\n'))
}

fn runs_fish(ctx: &Ctx, sid: u64) -> bool {
    let Some(s) = ctx.obs.session(sid) else { return false };
    let Some(id) = s.shell else { return false };
    ctx.obs.table.get(id.pid).is_some_and(|r| r.comm.trim_start_matches('-') == "fish")
}

pub(super) fn run(conn: &mut Conn, ctx: &Ctx, a: &RunArgs) -> Result<Value, Reject> {
    let sid = target(ctx, &a.target)?;
    gate_typing(conn, sid, a.force)?;
    conn.need_caps(CAP_TYPE_AND_WATCH)?;
    let text = run_text(&a.command, runs_fish(ctx, sid));
    admit_bytes(conn, ctx, sid, text.len())?;
    prompt_first(conn, ctx.caller, sid, PROMPT_WITHIN)?;
    let body = WatchBody { watch_id: 0, kind: WatchKind::CommandEnd, since_pos: 0, quiet_ms: 0, text: String::new() };
    let body = conn.add_watch(sid, ctx.caller, body)?;
    let t = PaneType { req_id: 0, sid, paste: true, enter: true, only_at_shell: true, bytes: text.clone().into_bytes(), watch: Some(body.clone()) };
    let deadline = Instant::now() + Duration::from_secs(a.timeout_s.into());
    let out = type_into(conn, sid, t).and_then(|typed| {
        let fired = conn.wait_registered(sid, &body, typed.pos, deadline)?;
        Ok((typed, fired))
    });
    conn.end_watch(sid, body.watch_id);
    if let Ok((_, Some(f))) = &out
        && f.reason == WatchReason::Met
    {
        conn.hub.links.command_ended(sid, Instant::now(), f.pos);
    }
    let (echo_off, result) = match &out {
        Ok((t, Some(f))) => (t.echo_off, f.status.map_or("ended".into(), |s| format!("exit {s}"))),
        Ok((t, None)) => (t.echo_off, "timeout".into()),
        Err(r) => (false, r.code.as_str().to_string()),
    };
    audit(conn, ctx, "pane.run", sid, Some(a.command.clone().into_bytes()), echo_off, &result);
    let (typed, fired) = out?;
    let mut v = screen_of(sid, 0, Some(fired.map_or(typed.pos, |f| f.pos)), &ctx.obs);
    match fired {
        Some(f) => {
            v.set("reason", super::ops_read::reason_word(f.reason, false).into());
            if let Some(st) = f.status {
                v.set("exit_status", i64::from(st).into());
            }
        }
        None => v.set("reason", "timeout".into()),
    }
    Ok(v)
}
