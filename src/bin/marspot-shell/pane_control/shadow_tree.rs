//! The tree of shadow panes, read from each session's `shadow_of`
//! (0: a pane in a window).  The registry is written by the sessions,
//! so a chain can be broken or, through a bad move, loop; every walk
//! here stops after `WALK_MAX` steps.

use marspot_peer::control::{ErrorCode, Reject};

use super::config::Config;

const WALK_MAX: usize = 16;

/// The pane at the top of `sid`'s chain of shadow parents, and how many
/// shadow levels down `sid` is.
pub(super) fn root_and_depth(parent_of: &dyn Fn(u64) -> u64, sid: u64) -> (u64, usize) {
    let (mut at, mut depth) = (sid, 0);
    while let Some(parent) = Some(parent_of(at)).filter(|p| *p != 0) {
        at = parent;
        depth += 1;
        if depth > WALK_MAX {
            break;
        }
    }
    (at, depth)
}

/// `sid` is `top` or somewhere below it.
pub(super) fn is_under(parent_of: &dyn Fn(u64) -> u64, sid: u64, top: u64) -> bool {
    let mut at = sid;
    for _ in 0..=WALK_MAX {
        if at == top {
            return true;
        }
        at = parent_of(at);
        if at == 0 {
            return false;
        }
    }
    false
}

/// How many of `shadows` sit below `sid`, and how many levels the
/// deepest of them is below it.
fn subtree(parent_of: &dyn Fn(u64) -> u64, shadows: &[u64], sid: u64) -> (usize, usize) {
    let mut n = 0;
    let mut deepest = 0;
    for &s in shadows {
        if s == sid {
            continue;
        }
        let mut at = s;
        for level in 1..=WALK_MAX {
            at = parent_of(at);
            if at == 0 {
                break;
            }
            if at == sid {
                n += 1;
                deepest = deepest.max(level);
                break;
            }
        }
    }
    (n, deepest)
}

/// The count limits for making `sid` (a pane in a window, with any
/// shadows of its own) a shadow of `host`: no loop, and the whole of
/// `sid`'s tree counted under `host`'s top pane and nesting depth.
pub(super) fn check_adopt(cfg: &Config, parent_of: &dyn Fn(u64) -> u64, shadows: &[u64], host: u64, sid: u64) -> Result<(), Reject> {
    if is_under(parent_of, host, sid) {
        return Err(Reject::new(ErrorCode::BadRequest, "a pane cannot become the shadow of its own shadow"));
    }
    let (n, d) = subtree(parent_of, shadows, sid);
    let (root, host_depth) = root_and_depth(parent_of, host);
    // shadows already under the root, leaving out sid's tree, which moves
    let under_root = shadows
        .iter()
        .filter(|&&s| !is_under(parent_of, s, sid) && root_and_depth(parent_of, s).0 == root)
        .count();
    let total = shadows.iter().filter(|&&s| s != sid).count();
    if total + 1 > cfg.shadows_total {
        return Err(Reject::new(ErrorCode::LimitCount, format!("{total} shadow panes exist, the most at once")));
    }
    if under_root + 1 + n > cfg.shadows_per_parent {
        return Err(Reject::new(
            ErrorCode::LimitCount,
            format!("{under_root} shadow panes under this pane and {} more would come, past the most", 1 + n),
        ));
    }
    if host_depth + 1 + d > cfg.shadow_depth {
        return Err(Reject::new(ErrorCode::LimitCount, format!("shadow panes nest {} deep at most", cfg.shadow_depth)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree(edges: &[(u64, u64)]) -> (impl Fn(u64) -> u64 + use<>, Vec<u64>) {
        let m: HashMap<u64, u64> = edges.iter().copied().collect();
        let shadows = edges.iter().map(|e| e.0).collect();
        (move |s| m.get(&s).copied().unwrap_or(0), shadows)
    }

    #[test]
    fn shadow_adopt_refuses_a_loop() {
        // 2 is a shadow of 1; making 1 a shadow of 2 would close a loop
        let (p, s) = tree(&[(2, 1), (3, 2)]);
        let cfg = Config::default();
        assert_eq!(check_adopt(&cfg, &p, &s, 2, 1).unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(check_adopt(&cfg, &p, &s, 3, 1).unwrap_err().code, ErrorCode::BadRequest);
        // a broken loop in the registry still ends
        let (p, s) = tree(&[(7, 8), (8, 7)]);
        assert_eq!(check_adopt(&cfg, &p, &s, 7, 9).unwrap_err().code, ErrorCode::LimitCount);
    }

    #[test]
    fn shadow_adopt_counts_the_moved_tree() {
        // the default total follows the machine's memory; 8 must fit
        let cfg = Config { shadows_total: 24, ..Config::default() };
        // host 100 has 5 shadows; pane 1 has 2 shadows of its own: 5 + 1 + 2 = 8
        let mut edges: Vec<(u64, u64)> = (101..=105).map(|s| (s, 100)).collect();
        edges.extend([(11, 1), (12, 1)]);
        let (p, s) = tree(&edges);
        check_adopt(&cfg, &p, &s, 100, 1).unwrap();
        edges.push((106, 100));
        let (p, s) = tree(&edges);
        assert_eq!(check_adopt(&cfg, &p, &s, 100, 1).unwrap_err().code, ErrorCode::LimitCount);
    }

    #[test]
    fn shadow_adopt_counts_the_moved_depth() {
        let cfg = Config::default();
        // 1 has a shadow 11: under a window pane 100 that is depth 2, allowed
        let (p, s) = tree(&[(11, 1)]);
        check_adopt(&cfg, &p, &s, 100, 1).unwrap();
        // under 101 (a shadow of 100) it would be depth 3
        let (p, s) = tree(&[(11, 1), (101, 100)]);
        assert_eq!(check_adopt(&cfg, &p, &s, 101, 1).unwrap_err().code, ErrorCode::LimitCount);
        // a leaf pane under 101 is depth 2
        let (p, s) = tree(&[(101, 100)]);
        check_adopt(&cfg, &p, &s, 101, 1).unwrap();
        assert_eq!(root_and_depth(&p, 101), (100, 1));
    }

    #[test]
    fn shadow_adopt_counts_the_total() {
        let cfg = Config::default();
        let edges: Vec<(u64, u64)> = (0..cfg.shadows_total as u64).map(|i| (200 + i, 300 + i)).collect();
        let (p, s) = tree(&edges);
        assert_eq!(check_adopt(&cfg, &p, &s, 100, 1).unwrap_err().code, ErrorCode::LimitCount);
    }
}
