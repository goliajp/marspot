use std::path::{Path, PathBuf};
use std::sync::Arc;

use marspot_peer::control::AgentKind::{self, Claude, Codex};

use super::Shared;
use super::dialogs::{Ask, Dialog, Look, Press, look};
use super::first_screens::First;
use super::launch;
use super::notify::{self, Stop, Verdict};
use super::passing::{Keys, drawn, take_enter};
use super::table::Event;
use super::tests::{feed, shared_at, started, temp};
use super::work::Job;

fn dialog(ask: Ask, cursor: usize, dir: &str) -> Dialog {
    let dirs = if ask == Ask::Trust { vec![vec![dir.to_string()]] } else { Vec::new() };
    Dialog { ask, options: ["No, exit", "maybe", "Yes, I accept"].map(String::from).to_vec(), cursor, yes: 2, dirs }
}

/// A helper on disk as `mode` started it, in `dir`.
fn helper(tag: &str, agent: AgentKind, mode: &str) -> (PathBuf, Arc<Shared>, First) {
    let dir = temp(tag);
    let (s, _rx) = shared_at(&dir);
    let mut st = started(10, "shell", agent);
    st.mode = mode.into();
    feed(&s, 1, vec![Event::Started(st), Event::TaskDelivered]);
    std::fs::create_dir_all(s.dir_of(1)).unwrap();
    let f = First { epoch: s.epoch(), s: Arc::clone(&s), sid: 1, agent };
    (dir, s, f)
}

#[test]
fn helpers_a_key_not_taken_yet_is_followed_by_none() {
    let at = |cursor| dialog(Ask::Trust, cursor, "/x");
    let mut k = Keys::default();
    assert_eq!(k.next(&at(0)), Some(Press::Down));
    assert_eq!(k.next(&at(0)), None, "the cursor has not moved");
    assert_eq!(k.next(&at(1)), Some(Press::Down));
    assert_eq!(k.next(&at(1)), None);
    assert_eq!(k.next(&at(2)), Some(Press::Enter));
    for c in [2, 0, 1, 2] {
        assert_eq!(k.next(&at(c)), None, "one Enter, and no key after it");
    }
    // a cursor that keeps moving elsewhere is not chased for ever
    let mut k = Keys::default();
    let n = (0..100).filter(|i| k.next(&at(i % 2)).is_some()).count();
    assert!((2..10).contains(&n), "{n}");
    let mut k = Keys::default();
    k.end();
    assert!(k.ended() && k.next(&at(0)).is_none());
}

#[test]
fn helpers_the_enter_of_a_dialog_is_taken_once_for_good() {
    let dir = temp("enter");
    assert!(take_enter(&dir, Ask::Trust));
    assert!(!take_enter(&dir, Ask::Trust));
    assert!(take_enter(&dir, Ask::Bypass), "each dialog has its own");
    assert!(!take_enter(&dir.join("gone"), Ask::Trust), "none that cannot be noted");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_only_what_the_agent_drew_is_a_screen() {
    let dir = Path::new("/Users/x/Library/Caches/marspot state/helpers/12");
    let echo = launch::start_line(dir);
    assert!(!drawn("", &echo, "") && !drawn("\n   \n", &echo, ""));
    // the prompt and the line typed at it, wrapped at any width
    let row = format!("~/proj (main) % {echo}");
    for w in 10..=row.chars().count() {
        let c: Vec<char> = row.chars().collect();
        let rows: Vec<String> = c.chunks(w).map(|r| r.iter().collect::<String>().trim_end().to_string()).collect();
        let screen = format!("motd\n{}\n\n", rows.join("\n"));
        assert!(!drawn(&screen, &echo, ""), "{w}\n{screen}");
        assert!(drawn(&format!("{screen}Loading\n"), &echo, ""), "{w}");
    }
    assert!(!drawn(&format!("% {echo}      12:30\n"), &echo, ""), "a right-hand prompt is the shell's");
    // the line scrolled off or cleared
    assert!(drawn("Loading", &echo, ""));
    // after an Enter: what was up before it is not new
    let before = format!("% {echo}\n banner\n ❯ Yes, I trust this folder\n");
    assert!(!drawn(&before, &echo, &before) && !drawn(" banner\n", &echo, &before));
    assert!(drawn(" banner\n ready\n", &echo, &before));
}

#[test]
fn helpers_an_undrawn_screen_is_no_stop_and_hides_no_dialog() {
    let (dir, s, f) = helper("undrawn", Claude, "auto");
    let echo = format!("% {}\n", launch::start_line(&s.dir_of(1)));
    assert!(f.stop_at(&echo, &Look::Other, "").is_none());
    assert!(f.stop_at("", &Look::Other, "").is_none());
    let loading = format!("{echo}Loading\n");
    assert!(matches!(f.stop_at(&loading, &Look::Other, ""), Some(Job::Stopped { stop: Stop::Blocked, by_screen: true, .. })));
    assert!(f.stop_at(&loading, &Look::Other, &loading).is_none(), "nothing drawn since the Enter");
    feed(&s, 1, vec![Event::BlockedNoticed]);
    let tells = |l: &Look| {
        let Some(Job::Stopped { stop, by_screen, .. }) = f.stop_at("", l, "") else { panic!("no stop") };
        notify::verdict(&s.table().map[&1], stop, by_screen, super::records::now_ms())
    };
    let (trust, bypass) = (Look::Ask(dialog(Ask::Trust, 0, "/etc")), Look::Ask(dialog(Ask::Bypass, 0, "")));
    assert_eq!(tells(&trust), Verdict::Send, "a generic notice went before");
    assert_eq!(tells(&trust), Verdict::Skip);
    // the cursor moved: the same dialog
    assert_eq!(tells(&Look::Ask(dialog(Ask::Trust, 2, "/etc"))), Verdict::Skip);
    assert_eq!(tells(&bypass), Verdict::Send);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_the_watch_judges_codex_only_once_it_drew() {
    let (dir, s, f) = helper("codexdrawn", Codex, "auto");
    let echo = format!("% {}\n", launch::start_line(&s.dir_of(1)));
    assert!(f.stop_at(&format!("{echo}Loading\n"), &Look::Other, "").is_none());
    assert!(matches!(f.stop_at(&echo, &Look::Ask(dialog(Ask::Trust, 0, "/")), ""), Some(Job::Stopped { .. })));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_bypass_is_accepted_only_when_asked_for() {
    for (mode, ok) in [
        ("auto", true),
        ("--dangerously-skip-permissions", true),
        ("--verbose --permission-mode bypassPermissions", true),
        ("--permission-mode=bypassPermissions", true),
        ("ask", false),
        ("--permission-mode plan", false),
        ("--permission-mode acceptEdits bypassPermissions", false),
        ("automatic", false),
    ] {
        let (dir, _s, f) = helper("bypass", Claude, mode);
        assert_eq!(f.refusal(&dialog(Ask::Bypass, 0, "")).is_none(), ok, "{mode}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

const NOTE: &str = "Note: You’re in a subdirectory of a Git project. Trusting will apply to the repository root:";

fn codex(dir: &str, note: &str, root: &str) -> Dialog {
    let screen = format!(
        "\n  Folder access\n  {dir}\n\n{note}\n  {root}\n\n  Trust this folder? Codex can read, edit, and run files here.\n\n\
         › 1. Trust and continue\n  2. Back to Agent Command Center\n\n  enter continue · esc back\n"
    );
    match look(Codex, &screen, None) {
        Look::Ask(d) => d,
        other => panic!("{other:?} for\n{screen}"),
    }
}

#[test]
fn helpers_codex_note_on_a_root_is_seen_however_it_wraps() {
    let root = std::fs::canonicalize(temp("codexroot")).unwrap();
    let dir = root.join("sub");
    std::fs::create_dir_all(&dir).unwrap();
    let (d, r) = (dir.to_str().unwrap(), root.to_str().unwrap());
    let words: Vec<&str> = NOTE.split(' ').collect();
    for n in 1..=words.len() {
        let note: Vec<String> = words.chunks(n).map(|c| format!("  {}", c.join(" "))).collect();
        assert!(!codex(d, &note.join("\n"), r).is_about(&dir), "{n} words a row");
        // in the middle of a word too
        let c: Vec<char> = NOTE.chars().collect();
        let cut: Vec<String> = c.chunks(n + 7).map(|r| format!("  {}", r.iter().collect::<String>().trim())).collect();
        assert!(!codex(d, &cut.join("\n"), r).is_about(&dir), "{} columns", n + 7);
    }
    assert!(codex(d, "", "").is_about(&dir));
    // a root that can be read, and is the directory itself
    assert!(codex(d, &format!("  {NOTE}"), d).is_about(&dir));
    std::fs::remove_dir_all(&root).unwrap();
}
