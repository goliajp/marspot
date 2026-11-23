//! What a pane is called, and how to say which one you mean.
//!
//! Panes are not named by anyone.  A pane's base name is its working
//! directory's last component, and every pane has a four-digit code
//! that stays with it for life (`marspot_term::pane_code`); together
//! they are its full name, `spg#3381`.
//!
//! What is drawn follows one rule: a pane whose base no other pane has
//! shows the base alone, and when several share a base every one of
//! them shows its full name — never "the first one keeps the plain
//! name".  The full name addresses the pane either way, and where the
//! pane sits on screen plays no part, so moving a pane or closing its
//! neighbour changes nobody's code.
//!
//! Lives in the library because two layers need the same answer: L1
//! resolves `--send spg#3381`, L2 draws the name on the pane.

use marspot_peer::resolve::Target;

/// A pane, as everything that names one sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRef {
    pub sid: u64,
    pub cwd: String,
    /// None only when the code file could not be read or written.
    pub code: Option<u16>,
    /// A shadow pane: not drawn in any window, usually a helper in its
    /// parent's directory.
    pub shadow: bool,
}

impl PaneRef {
    pub fn new(sid: u64, cwd: impl Into<String>) -> Self {
        Self { sid, cwd: cwd.into(), code: marspot_term::pane_code::code_for(sid).ok(), shadow: false }
    }

    pub fn with_shadow(self, shadow: bool) -> Self {
        Self { shadow, ..self }
    }

    /// `base#code`.
    pub fn full_name(&self) -> String {
        match self.code {
            Some(code) => format!("{}#{code}", base_name(&self.cwd)),
            None => base_name(&self.cwd),
        }
    }
}

/// Every pane's display name, in input order.
///
/// A shadow pane always shows its full name and is not counted when
/// asking whether a base is shared: a helper opened in its parent's
/// directory must not turn the parent's `spg` into `spg#3381`.
pub fn assign(panes: &[PaneRef]) -> Vec<(u64, String)> {
    let bases: Vec<String> = panes.iter().map(|p| base_name(&p.cwd)).collect();
    let shown = |base: &String| panes.iter().zip(&bases).filter(|(p, b)| !p.shadow && *b == base).count();
    panes
        .iter()
        .zip(&bases)
        .map(|(p, base)| (p.sid, if p.shadow || shown(base) > 1 { p.full_name() } else { base.clone() }))
        .collect()
}

/// The name a directory gives a pane.  A `#` in it would read as the
/// start of a code, so it becomes `-`.
pub fn base_name(cwd: &str) -> String {
    let b = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if b.is_empty() {
        "?".to_string()
    } else {
        b.replace('#', "-")
    }
}

/// Which pane `target` means, in the forms `marspot_peer::resolve`
/// takes.  `kind_of` says which agent the pane with a given code runs.
///
/// `Err` carries a message meant to be read: an ambiguous name lists
/// the candidates by full name.  Guessing is never an option here; the
/// cost of guessing wrong is text typed into someone else's session.
pub fn resolve(target: &str, panes: &[PaneRef], kind_of: impl Fn(u16) -> Option<String>) -> Result<u64, String> {
    let bases: Vec<String> = panes.iter().map(|p| base_name(&p.cwd)).collect();
    let kinds: Vec<Option<String>> = panes.iter().map(|p| p.code.and_then(&kind_of)).collect();
    let targets: Vec<Target> = (0..panes.len()).map(|i| Target { base: &bases[i], code: panes[i].code, kind: kinds[i].as_deref(), shadow: panes[i].shadow }).collect();
    match marspot_peer::resolve::resolve(target, &targets) {
        Ok(hit) => Ok(panes[hit.index].sid),
        Err(miss) => Err(miss.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(sid: u64, cwd: &str, code: u16) -> PaneRef {
        PaneRef { sid, cwd: cwd.into(), code: Some(code), shadow: false }
    }

    fn name_of(panes: &[PaneRef], sid: u64) -> String {
        assign(panes).into_iter().find(|(s, _)| *s == sid).map(|(_, n)| n).unwrap_or_default()
    }

    fn no_agents(_: u16) -> Option<String> {
        None
    }

    #[test]
    fn a_lone_pane_shows_its_base_and_answers_to_its_full_name() {
        let ps = vec![pane(390, "/w/goliajp/marspot", 1024)];
        assert_eq!(name_of(&ps, 390), "marspot");
        assert_eq!(ps[0].full_name(), "marspot#1024");
        let claude = |code: u16| (code == 1024).then(|| "claude".to_string());
        for to in ["marspot", "marspot#1024", "claude:marspot"] {
            assert_eq!(resolve(to, &ps, claude), Ok(390), "{to}");
        }
    }

    #[test]
    fn panes_sharing_a_base_all_show_their_full_names() {
        let ps = vec![pane(390, "/w/goliajp/marspot", 1024), pane(412, "/w/stables/marspot", 9906), pane(413, "/w/goliajp/alt", 3381)];
        assert_eq!(name_of(&ps, 390), "marspot#1024");
        assert_eq!(name_of(&ps, 412), "marspot#9906");
        assert_eq!(name_of(&ps, 413), "alt");
        let kinds = |code: u16| match code {
            1024 => Some("claude".to_string()),
            9906 => Some("codex".to_string()),
            _ => None,
        };
        let e = resolve("marspot", &ps, kinds).unwrap_err();
        assert!(e.contains("marspot#1024 (claude), marspot#9906 (codex)"), "{e}");
        assert_eq!(resolve("claude:marspot", &ps, kinds), Ok(390));
        assert_eq!(resolve("codex:marspot", &ps, kinds), Ok(412));
        assert_eq!(resolve("marspot#1024", &ps, kinds), Ok(390));
        assert_eq!(resolve("marspot#9906", &ps, kinds), Ok(412));
        // without an agent the plain panes are still told apart by code
        assert_eq!(resolve("marspot#9906", &ps, no_agents), Ok(412));
    }

    #[test]
    fn closing_one_of_two_leaves_the_other_its_base_and_its_code() {
        let two = vec![pane(10, "/a/dup", 1111), pane(30, "/c/dup", 3333)];
        assert_eq!(name_of(&two, 10), "dup#1111");
        let one: Vec<_> = two.into_iter().filter(|p| p.sid == 10).collect();
        assert_eq!(name_of(&one, 10), "dup");
        assert_eq!(resolve("dup#1111", &one, no_agents), Ok(10));
        assert!(resolve("dup#3333", &one, no_agents).unwrap_err().starts_with("no pane is named \"dup#3333\""));
    }

    #[test]
    fn a_pane_that_changes_directory_keeps_its_code() {
        let before = vec![pane(10, "/w/marspot", 1024)];
        let after = vec![pane(10, "/w/alt", 1024)];
        assert_eq!((name_of(&before, 10), name_of(&after, 10)), ("marspot".to_string(), "alt".to_string()));
        assert_eq!(after[0].full_name(), "alt#1024");
        assert_eq!(resolve("marspot#1024", &after, no_agents), Ok(10), "the full name it had still reaches it");
    }

    /// Where a pane sits is not an input: listing the same panes in
    /// another order, or without a neighbour, names each as before.
    #[test]
    fn moving_or_closing_panes_changes_no_name() {
        let ps = vec![pane(10, "/w/dup", 1111), pane(20, "/w/dup", 2222), pane(30, "/w/dup", 3333)];
        let mut moved = ps.clone();
        moved.swap(0, 2);
        for sid in [10, 20, 30] {
            assert_eq!(name_of(&ps, sid), name_of(&moved, sid));
        }
        let fewer: Vec<_> = ps.iter().filter(|p| p.sid != 20).cloned().collect();
        assert_eq!(name_of(&fewer, 10), "dup#1111");
        assert_eq!(name_of(&fewer, 30), "dup#3333");
    }

    #[test]
    fn a_different_directory_is_not_a_rival() {
        let ps = vec![pane(390, "/w/goliajp/spg", 1024), pane(413, "/w/goliajp/spg-old", 2048)];
        assert_eq!(name_of(&ps, 390), "spg");
        assert_eq!(name_of(&ps, 413), "spg-old");
        assert_eq!(resolve("spg", &ps, no_agents), Ok(390));
        // a path tail or a part of a name is a guess, and is refused
        assert!(resolve("goliajp/spg", &ps, no_agents).is_err());
        assert!(resolve("old", &ps, no_agents).is_err());
    }

    fn shadow(sid: u64, cwd: &str, code: u16) -> PaneRef {
        pane(sid, cwd, code).with_shadow(true)
    }

    #[test]
    fn a_shadow_is_not_counted_as_a_rival() {
        let ps = vec![pane(390, "/w/goliajp/spg", 1024), shadow(391, "/w/goliajp/spg", 3381)];
        assert_eq!(name_of(&ps, 390), "spg", "opening a helper leaves the parent's name alone");
        assert_eq!(resolve("spg", &ps, no_agents), Ok(390));
        assert_eq!(resolve("spg#3381", &ps, no_agents), Ok(391));
    }

    #[test]
    fn a_shadow_always_shows_its_full_name() {
        let ps = vec![shadow(391, "/w/goliajp/spg", 3381)];
        assert_eq!(name_of(&ps, 391), "spg#3381");
        assert_eq!(resolve("spg#3381", &ps, no_agents), Ok(391));
        assert!(resolve("spg", &ps, no_agents).unwrap_err().starts_with("no pane is named \"spg\""));
    }

    #[test]
    fn two_shadows_in_one_directory_keep_their_own_full_names() {
        let ps = vec![shadow(391, "/w/spg", 3381), shadow(392, "/w/spg", 4410)];
        assert_eq!(name_of(&ps, 391), "spg#3381");
        assert_eq!(name_of(&ps, 392), "spg#4410");
        assert_eq!(resolve("spg#4410", &ps, no_agents), Ok(392));
        let with_parent = [vec![pane(390, "/w/spg", 1024)], ps].concat();
        assert_eq!(name_of(&with_parent, 390), "spg");
    }

    #[test]
    fn a_window_pane_beside_a_shadow_keeps_its_short_name_until_a_real_rival_comes() {
        let ps = vec![shadow(391, "/w/spg", 3381), pane(500, "/x/spg", 5000)];
        assert_eq!(name_of(&ps, 500), "spg");
        let rival = [ps, vec![pane(501, "/y/spg", 6000)]].concat();
        assert_eq!(name_of(&rival, 500), "spg#5000");
        assert_eq!(name_of(&rival, 501), "spg#6000");
        assert_eq!(name_of(&rival, 391), "spg#3381");
    }

    #[test]
    fn odd_directories_still_get_a_name() {
        assert_eq!(base_name("/w/spg/"), "spg");
        assert_eq!(base_name("/"), "?");
        assert_eq!(base_name(""), "?");
        assert_eq!(base_name("/w/c#-notes"), "c--notes");
        let uncoded = PaneRef { sid: 1, cwd: "/w/spg".into(), code: None, shadow: false };
        assert_eq!(uncoded.full_name(), "spg");
    }
}
