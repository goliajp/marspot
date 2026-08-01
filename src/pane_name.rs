//! What a pane is called, and how to say which one you mean.
//!
//! Panes are not named by anyone.  A pane's name is its working
//! directory's last component, and when several panes share one they
//! are numbered — `spg#1`, `spg#2` — with the numbering derived on
//! every read rather than stored.  Nothing renames a pane, so nothing
//! can go stale: open a second `spg` and both are numbered from that
//! moment; close one and the survivor is `spg` again.
//!
//! The rules, in the order they matter:
//!
//! 1. **Unique by construction.** When a name is shared, *every* pane
//!    with it gets a number — never "the first one keeps the plain
//!    name".  A bare name must never quietly mean "whichever came
//!    first"; that is how text ends up in the wrong session.
//! 2. **The number follows the screen.** `#1` is the one further up
//!    and to the left — window, then row, then column, i.e. the order
//!    a person reads them in.  Ranking by creation order looked the
//!    same in a list and wrong on screen: `doracawl#2` sat to the left
//!    of `doracawl#1`, and nobody counts panes that way.  A pane with
//!    no cell of its own (overflowed into the sidebar) sorts after the
//!    ones on screen, by session id, so it still has a stable name.
//! 3. **`#1` is optional when it is the only one.** `spg` and `spg#1`
//!    both address a lone `spg`, so a caller that stored `spg#1`
//!    while there were two keeps working after one closes.
//!
//! Lives in the library because two layers need the same answer: L1
//! resolves `--send spg#2`, L2 draws the name on the pane.  Two
//! implementations of a naming rule is two naming rules.

/// A pane, as everything that names one sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRef {
    pub sid: u64,
    pub cwd: String,
    /// Where it sits: `(window, row, column)`, all 1-based, in the
    /// order they sort — window first, then down, then across.  `None`
    /// for a pane with no cell (more panes than the grid holds).
    pub at: Option<(usize, usize, usize)>,
}

impl PaneRef {
    pub fn new(sid: u64, cwd: impl Into<String>, at: Option<(usize, usize, usize)>) -> Self {
        Self { sid, cwd: cwd.into(), at }
    }
}

/// Every pane's display name.
///
/// Input order is preserved; the numbering does not depend on it.
pub fn assign(panes: &[PaneRef]) -> Vec<(u64, String)> {
    let mut groups: std::collections::HashMap<String, Vec<&PaneRef>> =
        std::collections::HashMap::new();
    for p in panes {
        groups.entry(base_name(&p.cwd)).or_default().push(p);
    }
    for peers in groups.values_mut() {
        // Reading order, and a pane with no cell after every pane that
        // has one — `None` must not sort first just because it is
        // smaller.
        peers.sort_by_key(|p| (p.at.is_none(), p.at.unwrap_or((0, 0, 0)), p.sid));
    }
    panes
        .iter()
        .map(|p| {
            let base = base_name(&p.cwd);
            let peers = &groups[&base];
            if peers.len() == 1 {
                (p.sid, base)
            } else {
                let k = peers.iter().position(|q| q.sid == p.sid).unwrap_or(0) + 1;
                (p.sid, format!("{base}#{k}"))
            }
        })
        .collect()
}

/// The name a directory gives a pane, before any numbering.
pub fn base_name(cwd: &str) -> String {
    let b = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if b.is_empty() {
        "?".to_string()
    } else {
        b.to_string()
    }
}

/// Which pane `target` means — by name, with or without its number.
///
/// `Err` carries a message meant to be read: an ambiguous name lists
/// the candidates with their ids and full names, so the next attempt is
/// a copy-paste.  Guessing is never an option here; the cost of
/// guessing wrong is text typed into someone else's session.
pub fn resolve(target: &str, panes: &[PaneRef]) -> Result<u64, String> {
    let t = target.trim().to_lowercase();
    if t.is_empty() {
        return Err("empty target".into());
    }
    let (base, want) = split_number(&t);
    let mut group: Vec<&PaneRef> = panes
        .iter()
        .filter(|p| base_name(&p.cwd).to_lowercase() == base)
        .collect();
    // The same order `assign` numbers by, or `#2` would mean one pane
    // in the title strip and another in `--send`.
    group.sort_by_key(|p| (p.at.is_none(), p.at.unwrap_or((0, 0, 0)), p.sid));
    if !group.is_empty() {
        return match want {
            Some(k) if k >= 1 && k <= group.len() => Ok(group[k - 1].sid),
            Some(k) => Err(format!(
                "there {} {} pane{} called {base:?}, so there is no #{k}",
                if group.len() == 1 { "is" } else { "are" },
                group.len(),
                if group.len() == 1 { "" } else { "s" },
            )),
            None if group.len() == 1 => Ok(group[0].sid),
            None => Err(ambiguous(
                target,
                &group.iter().map(|p| (p.sid, p.cwd.clone())).collect::<Vec<_>>(),
                panes,
            )),
        };
    }
    // Not a name: a path tail (`goliajp/spg` matches `/w/goliajp/spg`
    // but not `/w/goliajp/spg-old`, so adding a parent always narrows),
    // then a unique substring.
    let norm = |s: &str| s.trim_end_matches('/').to_lowercase();
    let hits: Vec<(u64, String)> = if t.contains('/') {
        panes
            .iter()
            .filter(|p| {
                let c = norm(&p.cwd);
                c == t || c.ends_with(&format!("/{t}"))
            })
            .map(|p| (p.sid, p.cwd.clone()))
            .collect()
    } else {
        panes
            .iter()
            .filter(|p| norm(&p.cwd).contains(&t))
            .map(|p| (p.sid, p.cwd.clone()))
            .collect()
    };
    match hits.len() {
        0 => Err(format!("no pane matches {target:?}")),
        1 => Ok(hits[0].0),
        _ => Err(ambiguous(target, &hits, panes)),
    }
}

/// `("spg", Some(2))` from `"spg#2"`; `("spg", None)` from `"spg"`.
fn split_number(t: &str) -> (String, Option<usize>) {
    match t.rsplit_once('#') {
        Some((base, n)) if !base.is_empty() => match n.parse::<usize>() {
            Ok(k) => (base.to_string(), Some(k)),
            Err(_) => (t.to_string(), None),
        },
        _ => (t.to_string(), None),
    }
}

fn ambiguous(target: &str, hits: &[(u64, String)], all: &[PaneRef]) -> String {
    let named: std::collections::HashMap<u64, String> = assign(all).into_iter().collect();
    let list = hits
        .iter()
        .map(|(sid, cwd)| {
            format!("  {sid}  {:<20}  {cwd}", named.get(sid).cloned().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{target:?} matches {} panes — say which:\n{list}", hits.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane on screen at `(window, row, column)`.
    fn at(sid: u64, cwd: &str, w: usize, y: usize, x: usize) -> PaneRef {
        PaneRef::new(sid, cwd, Some((w, y, x)))
    }

    /// A pane with no cell — more panes than the grid holds.
    fn offscreen(sid: u64, cwd: &str) -> PaneRef {
        PaneRef::new(sid, cwd, None)
    }

    /// Shorthand for the many cases where position is beside the
    /// point: one window, one row, laid out left to right.
    fn pane(sid: u64, cwd: &str) -> PaneRef {
        PaneRef::new(sid, cwd, Some((1, 1, sid as usize)))
    }

    fn name_of(panes: &[PaneRef], sid: u64) -> String {
        assign(panes)
            .into_iter()
            .find(|(s, _)| *s == sid)
            .map(|(_, n)| n)
            .unwrap_or_default()
    }

    /// One pane with a name has that name, plain.
    #[test]
    fn a_lone_pane_has_no_number() {
        let ps = vec![pane(390, "/w/goliajp/spg")];
        assert_eq!(name_of(&ps, 390), "spg");
    }

    /// …and answers to `spg#1` as well, because a caller that learned
    /// the numbered form while there were two must keep working after
    /// one closes.
    #[test]
    fn a_lone_pane_answers_to_its_number_too() {
        let ps = vec![pane(390, "/w/goliajp/spg")];
        assert_eq!(resolve("spg", &ps), Ok(390));
        assert_eq!(resolve("spg#1", &ps), Ok(390));
        // But not to a number that was never real.
        assert!(resolve("spg#2", &ps).unwrap_err().contains("no #2"));
    }

    /// Opening a second pane with the same name numbers **both**.
    ///
    /// Not "the first keeps `spg` and the newcomer is `spg#2`": a bare
    /// name must never silently mean the older one.
    #[test]
    fn adding_a_same_named_pane_numbers_every_one_of_them() {
        let one = vec![pane(390, "/w/goliajp/spg")];
        assert_eq!(name_of(&one, 390), "spg");

        let two = vec![pane(390, "/w/goliajp/spg"), pane(412, "/w/stables/spg")];
        assert_eq!(name_of(&two, 390), "spg#1", "the pane that was already there is renumbered");
        assert_eq!(name_of(&two, 412), "spg#2");
        // And the bare name now belongs to neither.
        let e = resolve("spg", &two).unwrap_err();
        assert!(e.contains("390") && e.contains("412"), "{e}");
        assert_eq!(resolve("spg#1", &two), Ok(390));
        assert_eq!(resolve("spg#2", &two), Ok(412));
    }

    /// A third one joins the end, and the first two keep their numbers.
    #[test]
    fn adding_a_third_appends_rather_than_reshuffling() {
        let two = vec![pane(10, "/a/dup"), pane(20, "/b/dup")];
        let three = vec![pane(10, "/a/dup"), pane(20, "/b/dup"), pane(30, "/c/dup")];
        assert_eq!(name_of(&two, 10), "dup#1");
        assert_eq!(name_of(&two, 20), "dup#2");
        assert_eq!(name_of(&three, 10), "dup#1", "unchanged");
        assert_eq!(name_of(&three, 20), "dup#2", "unchanged");
        assert_eq!(name_of(&three, 30), "dup#3");
    }

    /// The number follows the screen, not the clock.
    ///
    /// This is what the numbering got wrong at first: ranked by session
    /// id, `doracawl#2` sat to the *left* of `doracawl#1` — the older
    /// pane had been dragged to the right, and nobody counts panes by
    /// when they were opened.  Up and to the left is #1.
    #[test]
    fn the_number_follows_the_screen_not_creation_order() {
        // The exact case from the screen: 394 was created later but
        // sits in column 1; 386 is older and sits in column 2.
        let ps = vec![
            at(386, "/Users/x", 1, 4, 2),
            at(394, "/Users/x", 1, 4, 1),
        ];
        assert_eq!(name_of(&ps, 394), "x#1", "further left is #1");
        assert_eq!(name_of(&ps, 386), "x#2");
    }

    /// Reading order: window, then row, then column.
    #[test]
    fn reading_order_is_window_then_row_then_column() {
        let ps = vec![
            at(1, "/w/dup", 2, 1, 1), // second window — last
            at(2, "/w/dup", 1, 2, 1), // row 2
            at(3, "/w/dup", 1, 1, 3), // row 1, col 3
            at(4, "/w/dup", 1, 1, 1), // row 1, col 1 — first
        ];
        assert_eq!(name_of(&ps, 4), "dup#1");
        assert_eq!(name_of(&ps, 3), "dup#2");
        assert_eq!(name_of(&ps, 2), "dup#3");
        assert_eq!(name_of(&ps, 1), "dup#4");
    }

    /// A pane the grid could not fit still gets a name, and sorts
    /// after everything that is actually on screen — `None` must not
    /// come first just because it is the smaller value.
    #[test]
    fn a_pane_with_no_cell_sorts_after_the_ones_on_screen() {
        let ps = vec![
            offscreen(10, "/w/dup"),
            at(20, "/w/dup", 1, 1, 2),
            at(30, "/w/dup", 1, 1, 1),
        ];
        assert_eq!(name_of(&ps, 30), "dup#1");
        assert_eq!(name_of(&ps, 20), "dup#2");
        assert_eq!(name_of(&ps, 10), "dup#3", "off-screen goes last");
        assert_eq!(resolve("dup#3", &ps), Ok(10));
    }

    /// Moving a pane renumbers, because the number *is* the position.
    #[test]
    fn dragging_a_pane_past_its_twin_swaps_their_numbers() {
        let before = vec![
            at(10, "/w/dup", 1, 1, 1),
            at(20, "/w/dup", 1, 1, 2),
        ];
        assert_eq!(name_of(&before, 10), "dup#1");
        // They swap cells.
        let after = vec![
            at(10, "/w/dup", 1, 1, 2),
            at(20, "/w/dup", 1, 1, 1),
        ];
        assert_eq!(name_of(&after, 20), "dup#1", "whoever is left is #1");
        assert_eq!(name_of(&after, 10), "dup#2");
        assert_eq!(resolve("dup#1", &after), Ok(20), "and `--send` agrees");
    }

    /// Closing one closes the gap — the survivors renumber.
    #[test]
    fn removing_a_same_named_pane_renumbers_the_survivors() {
        let three = vec![pane(10, "/a/dup"), pane(20, "/b/dup"), pane(30, "/c/dup")];
        assert_eq!(name_of(&three, 30), "dup#3");

        // The middle one goes.
        let two: Vec<_> = three.into_iter().filter(|p| p.sid != 20).collect();
        assert_eq!(name_of(&two, 10), "dup#1");
        assert_eq!(name_of(&two, 30), "dup#2", "no gap is left behind");
        assert_eq!(resolve("dup#2", &two), Ok(30));
        assert!(resolve("dup#3", &two).unwrap_err().contains("no #3"));
    }

    /// Back to one, and the number disappears — but still answers to it.
    #[test]
    fn removing_the_last_rival_drops_the_number() {
        let two = vec![pane(10, "/a/dup"), pane(30, "/c/dup")];
        assert_eq!(name_of(&two, 10), "dup#1");

        let one: Vec<_> = two.into_iter().filter(|p| p.sid == 10).collect();
        assert_eq!(name_of(&one, 10), "dup", "a name with no rival is plain again");
        assert_eq!(resolve("dup", &one), Ok(10));
        assert_eq!(resolve("dup#1", &one), Ok(10), "the number it used to have still works");
    }

    /// Panes that merely *look* related are not a group: numbering is
    /// per exact name.
    #[test]
    fn a_different_directory_is_not_a_rival() {
        let ps = vec![pane(390, "/w/goliajp/spg"), pane(413, "/w/goliajp/spg-old")];
        assert_eq!(name_of(&ps, 390), "spg", "not numbered — nothing else is called spg");
        assert_eq!(name_of(&ps, 413), "spg-old");
        assert_eq!(resolve("spg", &ps), Ok(390));
        assert_eq!(resolve("spg-old", &ps), Ok(413));
    }

    /// Two panes in the *same* directory cannot be told apart by path,
    /// which is exactly why the number exists.
    #[test]
    fn same_directory_panes_are_still_addressable() {
        let ps = vec![pane(386, "/Users/x"), pane(394, "/Users/x")];
        assert_eq!(name_of(&ps, 386), "x#1");
        assert_eq!(name_of(&ps, 394), "x#2");
        assert_eq!(resolve("x#2", &ps), Ok(394));
        let e = resolve("x", &ps).unwrap_err();
        assert!(e.contains("386") && e.contains("394"), "{e}");
    }

    /// A path tail addresses a pane whose bare name is taken, and is
    /// exact about segments.
    #[test]
    fn a_path_tail_narrows_by_whole_segments() {
        let ps = vec![
            pane(390, "/w/goliajp/spg"),
            pane(412, "/w/stables/spg"),
            pane(413, "/w/goliajp/spg-old"),
        ];
        assert_eq!(resolve("goliajp/spg", &ps), Ok(390));
        assert_eq!(resolve("stables/spg", &ps), Ok(412));
        assert_eq!(resolve("spg-old", &ps), Ok(413));
    }

    /// A directory that ends in a slash, and one that has no last
    /// component at all, still produce a usable name.
    #[test]
    fn odd_directories_still_get_a_name() {
        assert_eq!(base_name("/w/spg/"), "spg");
        assert_eq!(base_name("/"), "?");
        assert_eq!(base_name(""), "?");
    }

    /// A `#` that is not a number is part of the name, not a rank.
    #[test]
    fn a_hash_that_is_not_a_number_is_just_text() {
        assert_eq!(split_number("spg#2"), ("spg".into(), Some(2)));
        assert_eq!(split_number("spg#x"), ("spg#x".into(), None));
        assert_eq!(split_number("#2"), ("#2".into(), None));
        assert_eq!(split_number("spg"), ("spg".into(), None));
    }
}
