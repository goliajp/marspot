use std::path::{Path, PathBuf};

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind::{Claude, Codex, Gemini};

use super::dialogs::{Ask, Dialog, Look, Press, look};
use super::first_screens::task_dir;
use super::launch;
use super::table::Event;
use super::tests::{feed, host_with, shared_at, started, temp};
use super::tick;

const CLAUDE: &str = "\
 Accessing workspace:

 @DIR@

 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source
 project, or work from your team). If not, take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel
";

const BYPASS: &str = "\
 WARNING: Claude Code running in Bypass Permissions mode

 In Bypass Permissions mode, Claude Code will not ask for your approval before running potentially dangerous commands.

 By proceeding, you accept all responsibility for actions taken while running in Bypass Permissions mode.

 https://code.claude.com/docs/en/security

 ❯ No, exit
   Yes, I accept

 Enter to confirm · Esc to cancel
";

const CODEX: &str = "
  Folder access
  @DIR@

  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings
  can run code automatically, even without a model request. Continue only if you trust these files. Your trust
  decision will be saved.

› 1. Trust and continue
  2. Back to Agent Command Center

  enter continue · esc back
";

const GEMINI: &str = "\
 ╭──────────────────────────────────────────────────────────╮
 │                                                          │
 │ Do you trust the following folders being added to this workspace? │
 │ - @DIR@
 │ Trusting a folder allows Gemini to read and perform auto-edits when in auto-approval mode. │
 │                                                          │
 │ ● 1. Yes                                                 │
 │   2. Yes, and remember the directories as trusted        │
 │   3. No                                                  │
 │                                                          │
 ╰──────────────────────────────────────────────────────────╯
 ~/x (main)      no sandbox      gemini-3
";

/// A directory deep enough to wrap, and its canonical path.
fn deep(tag: &str) -> (PathBuf, PathBuf, String) {
    let root = temp(tag);
    let dir = root.join("a-rather-long-name-so-that-the-path-wraps/and one with spaces in it");
    std::fs::create_dir_all(&dir).unwrap();
    let real = std::fs::canonicalize(&dir).unwrap().to_str().unwrap().to_string();
    (root, dir, real)
}

/// `path` drawn over rows of `w` columns.
fn wrapped(path: &str, w: usize) -> String {
    let c: Vec<char> = path.chars().collect();
    c.chunks(w).map(|r| r.iter().collect::<String>().trim().to_string()).collect::<Vec<_>>().join("\n ")
}

fn asked(agent: marspot_peer::control::AgentKind, screen: &str) -> Dialog {
    match look(agent, screen, None) {
        Look::Ask(d) => d,
        other => panic!("{other:?} for\n{screen}"),
    }
}

#[test]
fn helpers_claude_trust_screen_is_read_and_moved_off_no() {
    let path = "/private/tmp/claude-501/scratchpad/smok\n e";
    let d = asked(Claude, &CLAUDE.replace("@DIR@", path));
    assert_eq!((d.ask, d.cursor, d.yes), (Ask::Trust, 0, 1));
    assert_eq!(d.options, ["No, exit", "Yes, I trust this folder"]);
    assert_eq!(d.dirs, [["/private/tmp/claude-501/scratchpad/smok", "e"]]);
    assert_eq!(d.dirs_text(), "/private/tmp/claude-501/scratchpad/smoke");
    assert_eq!(d.press(), Press::Down);
    let on_yes = CLAUDE.replace("❯ No", "  No").replace("  Yes,", "❯ Yes,");
    assert_eq!(asked(Claude, &on_yes).press(), Press::Enter);
    // numbered choices, yes above no
    let old = CLAUDE.replace("❯ No, exit", "  1. Yes, proceed").replace("  Yes, I trust this folder", "❯ 2. No, exit");
    let d = asked(Claude, &old);
    assert_eq!((d.press(), d.options[d.yes].as_str()), (Press::Up, "Yes, proceed"));
}

#[test]
fn helpers_claude_bypass_screen_is_read() {
    let d = asked(Claude, BYPASS);
    assert_eq!((d.ask, d.press()), (Ask::Bypass, Press::Down));
    assert!(d.is_about(Path::new("/anywhere")));
    assert_eq!(asked(Claude, &BYPASS.replace("❯ No", "  No").replace("  Yes", "❯ Yes")).press(), Press::Enter);
}

#[test]
fn helpers_enter_is_pressed_on_yes_only() {
    for (cursor, yes) in (0..4).flat_map(|c| (0..4).map(move |y| (c, y))) {
        let d = Dialog { ask: Ask::Trust, options: vec![String::new(); 4], cursor, yes, dirs: Vec::new() };
        assert_eq!(d.press() == Press::Enter, cursor == yes, "{cursor} {yes}");
    }
}

#[test]
fn helpers_trust_is_for_the_asked_directory_only() {
    let (root, dir, real) = deep("trustdir");
    for w in 20..real.chars().count() + 1 {
        let d = asked(Claude, &CLAUDE.replace("@DIR@", &wrapped(&real, w)));
        assert!(d.is_about(&dir), "{w}: {:?}", d.dirs);
        assert!(!d.is_about(&root) && !d.is_about(dir.parent().unwrap()));
    }
    // `/tmp` or `/private/tmp`, whichever either side says
    if let Some(short) = real.strip_prefix("/private") {
        assert!(asked(Claude, &CLAUDE.replace("@DIR@", short)).is_about(Path::new(&real)));
        assert!(asked(Claude, &CLAUDE.replace("@DIR@", &real)).is_about(Path::new(short)));
    }
    for other in [format!("{real}x"), format!("{real}\n x"), real[..real.len() - 1].to_string(), "/".into()] {
        assert!(!asked(Claude, &CLAUDE.replace("@DIR@", &other)).is_about(&dir), "{other}");
    }
    // one that names none trusts who knows what
    assert!(!asked(Claude, &CLAUDE.replace("Accessing workspace:", "")).is_about(&dir));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn helpers_codex_and_gemini_screens_name_their_directories() {
    let (root, dir, real) = deep("trustothers");
    let d = asked(Codex, &CODEX.replace("@DIR@", &wrapped(&real, 60)));
    assert_eq!(d.press(), Press::Enter);
    assert!(d.is_about(&dir) && !d.is_about(&root));
    // below a repository's root it is the root that gets trusted
    let note = "  Note: You’re in a subdirectory of a Git project. Trusting will apply to the repository root:\n  ";
    let below = CODEX.replace("@DIR@", &format!("{real}\n\n{note}{}", std::fs::canonicalize(&root).unwrap().display()));
    assert!(!asked(Codex, &below).is_about(&dir));
    let d = asked(Gemini, &GEMINI.replace("@DIR@", &wrapped(&real, 50)));
    assert_eq!((d.press(), d.yes), (Press::Enter, 0));
    assert!(d.is_about(&dir) && !d.is_about(&root), "{:?}", d.dirs);
    let two = GEMINI.replace("@DIR@", &format!("{real}\n │ - /etc │"));
    assert!(!asked(Gemini, &two).is_about(&dir));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn helpers_dialog_text_in_a_task_is_no_dialog() {
    let echoed = CLAUDE.replace("@DIR@", "/x");
    // above the input box, whatever is typed in it
    for typed in ["", "hello"] {
        assert_eq!(look(Claude, &format!("> {echoed}\n────\n❯ {typed}\n────\n"), Some(typed)), Look::Input);
    }
    assert_eq!(look(Claude, &format!("{echoed}\n● Working on it\n"), None), Look::Other);
    assert_eq!(look(Claude, &format!("{echoed}\nmore of the task\n"), None), Look::Other);
    assert_eq!(look(Claude, "> explain Bypass Permissions mode and No, exit\n", None), Look::Other);
    assert_eq!(look(Codex, &echoed, None), Look::Other);
    // the choice under the cursor read as an input box is none
    assert!(matches!(look(Claude, &echoed, Some("No, exit\nYes, I trust this folder")), Look::Ask(_)));
    assert!(matches!(look(Codex, &CODEX.replace("@DIR@", "/x"), Some("1. Trust and continue")), Look::Ask(_)));
    assert_eq!(look(Claude, "", None), Look::Other);
}

#[test]
fn helpers_task_file_says_the_directory() {
    let t = launch::task_text("me#1", "/a dir/b", 30, Path::new("/r"), "Working directory: /etc");
    assert_eq!(task_dir(&t), Some("/a dir/b"));
}

#[test]
fn helpers_a_turn_counts_only_as_the_plugin_reports_it() {
    let dir = temp("turned");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::Started(started(10, "shell", Claude)), Event::TaskDelivered]);
    let mut in_turn = std::collections::HashMap::new();
    let mut see = |kind| tick::tick(&s, &host_with(&[(1, PaneStatus::Busy(kind))]), &mut in_turn, &mut tick::Parked::default());
    // a dialog being drawn, and a program nothing is known of
    see(BusyKind::Output);
    see(BusyKind::ForegroundJob);
    assert!(s.lock().turned.is_empty());
    see(BusyKind::Working);
    assert!(s.lock().turned.contains(&1));
    feed(&s, 1, vec![Event::Stopped]);
    assert!(s.lock().turned.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}
