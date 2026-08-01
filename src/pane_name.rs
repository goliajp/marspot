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
//! 2. **Stable while the set is.** The number ranks by session id,
//!    which is creation order.  Adding or removing a pane renumbers
//!    the group and nothing else.
//! 3. **`#1` is optional when it is the only one.** `spg` and `spg#1`
//!    both address a lone `spg`, so a caller that stored `spg#1`
//!    while there were two keeps working after one closes.
//!
//! Lives in the library because two layers need the same answer: L1
//! resolves `--send spg#2`, L2 draws the name on the pane.  Two
//! implementations of a naming rule is two naming rules.

/// Every pane's display name, given `(session id, working directory)`.
///
/// Input order is preserved; the numbering does not depend on it.
pub fn assign(panes: &[(u64, String)]) -> Vec<(u64, String)> {
    let mut groups: std::collections::HashMap<String, Vec<u64>> = std::collections::HashMap::new();
    for (sid, cwd) in panes {
        groups.entry(base_name(cwd)).or_default().push(*sid);
    }
    for sids in groups.values_mut() {
        sids.sort_unstable();
    }
    panes
        .iter()
        .map(|(sid, cwd)| {
            let base = base_name(cwd);
            let peers = &groups[&base];
            if peers.len() == 1 {
                (*sid, base)
            } else {
                let k = peers.iter().position(|s| s == sid).unwrap_or(0) + 1;
                (*sid, format!("{base}#{k}"))
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
pub fn resolve(target: &str, panes: &[(u64, String)]) -> Result<u64, String> {
    let t = target.trim().to_lowercase();
    if t.is_empty() {
        return Err("empty target".into());
    }
    let (base, want) = split_number(&t);
    let mut group: Vec<(u64, String)> = panes
        .iter()
        .filter(|(_, cwd)| base_name(cwd).to_lowercase() == base)
        .cloned()
        .collect();
    group.sort_by_key(|(sid, _)| *sid);
    if !group.is_empty() {
        return match want {
            Some(k) if k >= 1 && k <= group.len() => Ok(group[k - 1].0),
            Some(k) => Err(format!(
                "there {} {} pane{} called {base:?}, so there is no #{k}",
                if group.len() == 1 { "is" } else { "are" },
                group.len(),
                if group.len() == 1 { "" } else { "s" },
            )),
            None if group.len() == 1 => Ok(group[0].0),
            None => Err(ambiguous(target, &group.iter().map(|(s, c)| (*s, c.clone())).collect::<Vec<_>>(), panes)),
        };
    }
    // Not a name: a path tail (`goliajp/spg` matches `/w/goliajp/spg`
    // but not `/w/goliajp/spg-old`, so adding a parent always narrows),
    // then a unique substring.
    let norm = |s: &str| s.trim_end_matches('/').to_lowercase();
    let hits: Vec<(u64, String)> = if t.contains('/') {
        panes
            .iter()
            .filter(|(_, cwd)| {
                let c = norm(cwd);
                c == t || c.ends_with(&format!("/{t}"))
            })
            .cloned()
            .collect()
    } else {
        panes.iter().filter(|(_, cwd)| norm(cwd).contains(&t)).cloned().collect()
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

fn ambiguous(target: &str, hits: &[(u64, String)], all: &[(u64, String)]) -> String {
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

    fn pane(sid: u64, cwd: &str) -> (u64, String) {
        (sid, cwd.to_string())
    }

    fn name_of(panes: &[(u64, String)], sid: u64) -> String {
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

    /// A pane created *earlier* but seen later still sorts first: the
    /// number ranks by session id, not by whatever order the caller
    /// happened to list them in.
    #[test]
    fn the_number_follows_creation_order_not_listing_order() {
        let listed_backwards = vec![pane(30, "/c/dup"), pane(10, "/a/dup"), pane(20, "/b/dup")];
        assert_eq!(name_of(&listed_backwards, 10), "dup#1");
        assert_eq!(name_of(&listed_backwards, 20), "dup#2");
        assert_eq!(name_of(&listed_backwards, 30), "dup#3");
    }

    /// Closing one closes the gap — the survivors renumber.
    #[test]
    fn removing_a_same_named_pane_renumbers_the_survivors() {
        let three = vec![pane(10, "/a/dup"), pane(20, "/b/dup"), pane(30, "/c/dup")];
        assert_eq!(name_of(&three, 30), "dup#3");

        // The middle one goes.
        let two: Vec<_> = three.into_iter().filter(|(s, _)| *s != 20).collect();
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

        let one: Vec<_> = two.into_iter().filter(|(s, _)| *s == 10).collect();
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
