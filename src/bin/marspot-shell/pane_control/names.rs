//! Which pane a caller means, and what it says.  Shared by the control
//! socket and the command socket (`marspot-shell --send / --read /
//! --panes`), so a name means one pane everywhere.

use marspot_peer::control::{ErrorCode, Reject};

use super::super::observe::Observation;

/// How a caller said which pane it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The session id — the one address that is never reused and never
    /// moves.  Everything else is a way of not having to know it.
    Id(u64),
    /// A pane's name: `spg`, its full name `spg#3381`, or either with
    /// the agent in front (`claude:spg`).
    Name(String),
    /// The cell a pane occupies: window `w`, column `x`, row `y`, all
    /// 1-based because a person counting panes starts at one.  It
    /// addresses *the pane in that slot*, whichever it is.
    Cell { w: usize, x: usize, y: usize },
}

/// Parse `w(2,1,3)` / `390` / `#390` / `spg` / `goliajp/spg`.
pub fn parse_target(target: &str) -> Result<Target, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("empty target".into());
    }
    if let Some(rest) = t.strip_prefix('w').or_else(|| t.strip_prefix('W'))
        && let Some(inner) = rest.trim().strip_prefix('(').and_then(|r| r.strip_suffix(')'))
    {
        let nums: Vec<&str> = inner.split(',').map(str::trim).collect();
        if nums.len() != 3 {
            return Err(format!("{target:?}: expected w(window, x, y)"));
        }
        let mut vals = [0usize; 3];
        for (i, n) in nums.iter().enumerate() {
            vals[i] = n.parse().map_err(|_| format!("{target:?}: {n:?} is not a number"))?;
            if vals[i] == 0 {
                return Err(format!("{target:?}: counts start at 1"));
            }
        }
        return Ok(Target::Cell { w: vals[0], x: vals[1], y: vals[2] });
    }
    let bare = t.strip_prefix('#').unwrap_or(t);
    if let Ok(sid) = bare.parse::<u64>() {
        return Ok(Target::Id(sid));
    }
    Ok(Target::Name(t.to_string()))
}

/// Every pane in `obs` as `(session id, cwd, title)`.  The cwd is read
/// live off the pane's shell rather than taken from the registry, whose
/// copy is from spawn time.  Only a live session's shell is read: a
/// dead one's pid may name any process by now.
pub fn live_panes(obs: &Observation) -> Vec<(u64, String, String)> {
    obs.sessions
        .iter()
        .map(|s| {
            let e = &s.entry;
            let cwd = s.shell.and_then(|id| marspot::pidtree::pane_cwd(id.pid));
            let cwd = cwd.map(|p| p.to_string_lossy().into_owned());
            (e.id, cwd.unwrap_or_else(|| e.cwd.clone()), e.title.clone())
        })
        .collect()
}

/// The working directory of pane `sid`: its shell's, else the one the
/// registry has.  One process looked at, not every pane's.
pub fn cwd_of(sid: u64, obs: &Observation) -> Option<String> {
    let s = obs.session(sid)?;
    let cwd = s.shell.and_then(|id| marspot::pidtree::pane_cwd(id.pid));
    Some(cwd.map_or_else(|| s.entry.cwd.clone(), |p| p.to_string_lossy().into_owned()))
}

/// Every pane with what naming needs, its shadow mark read from its
/// registry entry.  May read the code file, so not for the main
/// thread's hot paths.
pub fn pane_refs_in(obs: &Observation) -> Vec<marspot::pane_name::PaneRef> {
    // live_panes lists obs.sessions in order
    live_panes(obs)
        .into_iter()
        .zip(&obs.sessions)
        .map(|((sid, cwd, _), s)| marspot::pane_name::PaneRef::new(sid, cwd).with_shadow(super::panes::shadow_of(&s.entry) != 0))
        .collect()
}

/// `sid → (window, x, y)`, 1-based, from the layout L2 persists.
pub fn layout_cells() -> std::collections::HashMap<u64, (usize, usize, usize)> {
    let mut out = std::collections::HashMap::new();
    let Some(saved) = marspot::state::read() else {
        return out;
    };
    for (wi, win) in saved.windows.iter().enumerate() {
        let cols = win.grid_cols.max(1) as usize;
        for (i, p) in win.panes.iter().enumerate() {
            if p.sid != 0 {
                out.insert(p.sid, (wi + 1, i % cols + 1, i / cols + 1));
            }
        }
    }
    out
}

/// The pane occupying window `w`'s cell `(x, y)`, all 1-based, read from
/// the layout L2 persists: L1 owns the windows, L2 decides what sits
/// where.
pub fn pane_at_cell(w: usize, x: usize, y: usize) -> Result<u64, String> {
    let saved = marspot::state::read().ok_or_else(|| "no window layout on disk yet".to_string())?;
    let win = saved
        .windows
        .get(w - 1)
        .ok_or_else(|| format!("there is no window {w} (there are {})", saved.windows.len()))?;
    let (cols, rows) = (win.grid_cols as usize, win.grid_rows as usize);
    if x > cols || y > rows {
        return Err(format!("window {w} is {cols}×{rows}; ({x},{y}) is outside it"));
    }
    match win.panes.get((y - 1) * cols + (x - 1)) {
        Some(p) if p.sid != 0 => Ok(p.sid),
        _ => Err(format!("window {w} cell ({x},{y}) is empty")),
    }
}

/// Which pane a name means, in the forms agents use between
/// themselves; `Err((unknown, text))`, `unknown` when no pane fits at
/// all rather than several.
pub fn resolve_name_in(target: &str, panes: &[marspot::pane_name::PaneRef]) -> Result<u64, (bool, String)> {
    let table = marspot_peer::panes::live(&marspot_peer::peers::live_entries());
    let agent_of = |code| table.iter().filter(|p| p.has_inbox()).find(|p| p.code == Some(code)).and_then(|p| p.agent.clone());
    let bases: Vec<String> = panes.iter().map(|p| marspot::pane_name::base_name(&p.cwd)).collect();
    let kinds: Vec<Option<String>> = panes.iter().map(|p| p.code.and_then(agent_of)).collect();
    let targets: Vec<marspot_peer::resolve::Target> = (0..panes.len())
        .map(|i| marspot_peer::resolve::Target { base: &bases[i], code: panes[i].code, kind: kinds[i].as_deref(), shadow: panes[i].shadow })
        .collect();
    match marspot_peer::resolve::resolve(target, &targets) {
        Ok(hit) => Ok(panes[hit.index].sid),
        Err(miss) => Err((miss.unknown, miss.text)),
    }
}

/// Turn what a caller said into one pane of `obs`; the reason in words
/// when it names none or several.
pub fn resolve(target: &str, obs: &Observation) -> Result<u64, String> {
    resolve_coded(target, obs).map_err(|r| r.message)
}

/// [`resolve`] with the error word the control socket answers with.
pub fn resolve_coded(target: &str, obs: &Observation) -> Result<u64, Reject> {
    let panes = pane_refs_in(obs);
    let no_pane = |m: String| Reject::new(ErrorCode::NoSuchPane, m);
    match parse_target(target).map_err(no_pane)? {
        Target::Id(sid) if panes.iter().any(|p| p.sid == sid) => Ok(sid),
        Target::Id(sid) => Err(no_pane(format!("no pane with session id {sid}"))),
        Target::Name(n) => resolve_name_in(&n, &panes).map_err(|(unknown, text)| {
            let mut r = Reject::new(if unknown { ErrorCode::NoSuchPane } else { ErrorCode::AmbiguousPane }, text);
            if !unknown {
                r.candidates = panes.iter().map(|p| p.full_name()).filter(|f| f.starts_with(n.rsplit(':').next().unwrap_or(&n))).collect();
            }
            r
        }),
        Target::Cell { w, x, y } => pane_at_cell(w, x, y).map_err(no_pane),
    }
}

/// The full name (`dir#code`) of pane `sid` in `obs`.
pub fn full_name(sid: u64, obs: &Observation) -> String {
    cwd_of(sid, obs).map_or_else(|| sid.to_string(), |cwd| marspot::pane_name::PaneRef::new(sid, cwd).full_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three ways of saying which pane.
    #[test]
    fn every_address_form_parses() {
        assert_eq!(parse_target("390"), Ok(Target::Id(390)));
        assert_eq!(parse_target("#390"), Ok(Target::Id(390)));
        assert_eq!(parse_target("spg#3381"), Ok(Target::Name("spg#3381".into())));
        assert_eq!(parse_target("claude:spg"), Ok(Target::Name("claude:spg".into())));
        // a code only follows a name: a bare number is a session id
        // whatever its length
        assert_eq!(parse_target("#3381"), Ok(Target::Id(3381)));
        assert_eq!(parse_target("w(2,1,3)"), Ok(Target::Cell { w: 2, x: 1, y: 3 }));
        assert_eq!(parse_target(" W( 1 , 2 , 2 ) "), Ok(Target::Cell { w: 1, x: 2, y: 2 }));
        // Counting starts at one, so zero is a mistake worth naming.
        assert!(parse_target("w(0,1,1)").is_err());
        assert!(parse_target("w(1,1)").is_err());
        assert!(parse_target("").is_err());
    }
}
