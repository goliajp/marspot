//! Finding the profiles on this machine, and the badge menu that offers them.

use super::*;

fn tmphome(entries: &[(&str, bool)]) -> TempDir {
    let home = TempDir::new("cc-plugin-home");
    for (name, is_dir) in entries {
        let p = home.join(name);
        if *is_dir {
            fs::create_dir(&p).unwrap();
        } else {
            fs::File::create(&p).unwrap();
        }
    }
    home
}

#[test]
fn discover_profiles_finds_sorted_dirs() {
    let home = tmphome(&[
        (".claude-profile-4", true),
        (".claude-profile-1", true),
        (".claude-profile-2", true),
        (".claude", true),
        (".claude-profile-x", true),   // non-numeric suffix
        (".claude-profile-3", false),  // a file, not a dir
        ("Documents", true),
    ]);
    assert_eq!(discover_profiles_in(home.path()), vec![1, 2, 4]);
}

#[test]
fn discover_profiles_empty_when_none_exist() {
    let home = tmphome(&[(".claude", true)]);
    assert!(discover_profiles_in(home.path()).is_empty());
}

#[test]
fn short_model_normalises_ids_and_display_names() {
    assert_eq!(short_model("claude-fable-5"), "fable-5");
    assert_eq!(short_model("claude-opus-4-8"), "opus-4-8");
    assert_eq!(short_model("claude-sonnet-5"), "sonnet-5");
    assert_eq!(short_model("claude-haiku-4-5-20251001"), "haiku-4-5");
    assert_eq!(short_model("Fable 5"), "fable-5");
    assert_eq!(short_model("Opus 4.8"), "opus-4-8");
    assert_eq!(short_model("Default (recommended)"), "default");
    // ANSI-bold display name straight out of /model's stdout.
    assert_eq!(short_model("\u{1b}[1mFable 5\u{1b}[22m"), "fable-5");
}

#[test]
fn badge_menu_excludes_current_profile() {
    let items = badge_menu_for(4, &[1, 2, 3, 4]);
    assert_eq!(
        items
            .iter()
            .map(|i| (i.tag, i.label.clone()))
            .collect::<Vec<_>>(),
        [1u8, 2, 3].map(|n| (crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, n), format!("switch to P{n}"))).to_vec(),
    );
}

#[test]
fn badge_menu_from_p0_offers_every_profile() {
    // P0 (default .claude) isn't in the discovered set, so every
    // numbered profile is a target.
    let items = badge_menu_for(0, &[1, 2]);
    assert_eq!(items.len(), 2);
}

#[test]
fn badge_menu_single_profile_current_is_empty() {
    assert!(badge_menu_for(1, &[1]).is_empty());
    assert!(badge_menu_for(1, &[]).is_empty());
}

#[test]
fn discover_profiles_missing_home_is_empty() {
    let tmp = TempDir::new("cc-no-home");
    assert!(discover_profiles_in(&tmp.join("nonexistent")).is_empty());
}
