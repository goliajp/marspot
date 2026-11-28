//! A dry-run record: what decided a switch, written so the decision can
//! be made again from the record alone and compared with the one
//! written beside it.  Floats are written in their shortest form that
//! reads back to the same bits.

use super::{Boundary, Kind, Limits, Watched, Would, decide};

/// The switch and the boundary as `key=value` words.
pub fn words(w: &Watched, b: &Boundary) -> String {
    format!(
        "open={} dead={} exempt={} clean={} long={} lost={} gain={} noticed={} at={} turn={} live={} user={} quiet={}",
        w.open_at,
        w.dead_at,
        u8::from(w.exempt),
        w.clean_per_h,
        u8::from(w.long_background),
        w.lost_work,
        w.gain,
        w.noticed_at.map_or("-".to_string(), |t| t.to_string()),
        b.at,
        u8::from(b.turn_running),
        u8::from(b.live),
        u8::from(b.user),
        b.quiet_s,
    )
}

/// `words` read back.
pub fn parse(detail: &str) -> Option<(Watched, Boundary)> {
    let get = |k: &str| detail.split(' ').find_map(|x| x.strip_prefix(k)?.strip_prefix('='));
    let flag = |k: &str| get(k).map(|v| v == "1");
    let w = Watched {
        open_at: get("open")?.parse().ok()?,
        dead_at: get("dead")?.parse().ok()?,
        exempt: flag("exempt")?,
        clean_per_h: get("clean")?.parse().ok()?,
        long_background: flag("long")?,
        lost_work: get("lost")?.parse().ok()?,
        gain: get("gain")?.parse().ok()?,
        noticed_at: match get("noticed")? {
            "-" => None,
            v => Some(v.parse().ok()?),
        },
    };
    let b = Boundary { at: get("at")?.parse().ok()?, turn_running: flag("turn")?, live: flag("live")?, user: flag("user")?, quiet_s: get("quiet")?.parse().ok()? };
    Some((w, b))
}

fn token(would: Would) -> String {
    match would {
        Would::Wait => "wait".into(),
        Would::Switch(k) => format!("switch:{}", k.word()),
        Would::Notify(k) => format!("notify:{}", k.word()),
        Would::Cancel => "cancel".into(),
    }
}

fn of_token(t: &str) -> Option<Would> {
    let kind = |w: &str| [Kind::Clean, Kind::Short, Kind::Notice, Kind::Drain].into_iter().find(|k| k.word() == w);
    match t.split_once(':') {
        None if t == "wait" => Some(Would::Wait),
        None if t == "cancel" => Some(Would::Cancel),
        Some(("switch", k)) => kind(k).map(Would::Switch),
        Some(("notify", k)) => kind(k).map(Would::Notify),
        _ => None,
    }
}

/// The record of `would`, decided at `b` for `w` under `lim`: the
/// decision, the two limits the rule reads, and `words`.
pub fn record(w: &Watched, b: &Boundary, lim: &Limits, would: Would) -> String {
    format!("decided={} wait_min={} notice_working={} {}", token(would), lim.notice_wait_min, u8::from(lim.notice_working), words(w, b))
}

/// Whether `detail` (a `record`, possibly inside a longer line) decides
/// again to the decision it records; `None` when it does not read.
pub fn replay(detail: &str) -> Option<bool> {
    let get = |k: &str| detail.split(' ').find_map(|x| x.strip_prefix(k)?.strip_prefix('='));
    let recorded = of_token(get("decided")?)?;
    let lim = Limits { notice_wait_min: get("wait_min")?.parse().ok()?, notice_working: get("notice_working")? == "1", ..Limits::default() };
    let (w, b) = parse(detail)?;
    Some(decide(&w, &b, &lim) == recorded)
}
