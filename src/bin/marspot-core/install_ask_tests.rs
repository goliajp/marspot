//! The question before an agent CLI's first install, with keys going
//! to a real session behind it.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::helper_strip::host_tests::{app, Session};
use marspot::input::NamedKey;

fn answers(pairs: &[(&str, InstallConsent)]) -> InstallConsents {
    let mut c = InstallConsents::default();
    for (cli, to) in pairs {
        c.set(cli, *to);
    }
    c
}

#[test]
fn install_asks_are_shown_one_at_a_time_and_an_answered_cli_is_not_asked() {
    let mut q = InstallAsks::default();
    let none = InstallConsents::default();
    assert_eq!(q.shown(), None);
    assert!(q.offer("claude", "c1".into(), &none));
    assert!(q.offer("codex", "x1".into(), &none));
    assert!(q.offer("claude", "c2".into(), &none), "asked again: the text is replaced, not doubled");
    assert_eq!(q.shown(), Some("c2"));
    assert!(!q.offer("gemini", "g".into(), &answers(&[("gemini", InstallConsent::No)])), "answered no");
    assert!(!q.offer("npm", "n".into(), &none), "no such setting");
    assert!(q.prune(&answers(&[("claude", InstallConsent::Yes)])), "answered in the panel");
    assert_eq!(q.shown(), Some("x1"));
    assert!(!q.prune(&answers(&[("claude", InstallConsent::Yes)])), "nothing more went");
    assert!(q.prune(&answers(&[("claude", InstallConsent::Yes), ("codex", InstallConsent::No)])));
    assert_eq!(q.shown(), None);
    assert_eq!(q.up_since, None);
}

#[test]
fn install_ask_answers_are_only_ctrl_option_y_and_n() {
    let ev = |logical, text: Option<&str>| MarspotKeyEvent {
        state: KeyState::Pressed,
        logical,
        text: text.map(Into::into),
        ..Default::default()
    };
    let chord = Modifiers { control: true, alt: true, ..Default::default() };
    let y = ev(LogicalKey::Char('y'), Some("\u{19}"));
    let n = ev(LogicalKey::Char('n'), Some("\u{e}"));
    assert_eq!(answer_for(&y, chord), Some(InstallConsent::Yes));
    assert_eq!(answer_for(&n, chord), Some(InstallConsent::No));
    assert_eq!(answer_for(&ev(LogicalKey::Char('x'), None), chord), None);
    assert_eq!(answer_for(&ev(LogicalKey::Named(NamedKey::Escape), None), chord), None);
    for mods in [
        Modifiers::default(),
        Modifiers { control: true, ..Default::default() },
        Modifiers { alt: true, ..Default::default() },
        Modifiers { shift: true, ..chord },
        Modifiers { super_: true, ..chord },
    ] {
        assert_eq!(answer_for(&y, mods), None, "{mods:?}");
        assert_eq!(answer_for(&n, mods), None, "{mods:?}");
    }
    let full_width = ev(LogicalKey::Other, Some("\u{FF59}"));
    assert_eq!(answer_for(&full_width, Modifiers::default()), None, "committed text never answers");
}

fn key_with(app: &mut CoreApp, logical: LogicalKey, mods: Modifiers) {
    for state in [KeyState::Pressed, KeyState::Released] {
        let text = match &logical {
            LogicalKey::Char(c) if !mods.control => Some(c.to_string().into()),
            _ => None,
        };
        app.key(0, MarspotKeyEvent { state, logical: logical.clone(), text, ..Default::default() }, mods);
    }
}

fn key(app: &mut CoreApp, logical: LogicalKey) {
    key_with(app, logical, Modifiers::default());
}

const CHORD: Modifiers = Modifiers { control: true, alt: true, shift: false, super_: false };

/// An app with one session and the question for codex up.
fn asking(sb: &Sandbox) -> (CoreApp, Session) {
    let (mut app, s) = app(sb);
    marspot::settings::set_for_test(marspot::settings::Settings::default());
    app.install_ask(InstallAsk { cli: "codex".into(), place: "~/.codex".into(), megabytes: 740 });
    assert!(app.notices.install.shown().is_some());
    (app, s)
}

fn written() -> String {
    std::fs::read_to_string(marspot::settings::path()).unwrap_or_default()
}

fn unchanged() {
    assert!(!written().contains("agents.install.codex"), "{}", written());
    assert_eq!(marspot::settings::get().install_consent.of("codex"), Some(InstallConsent::Ask));
}

#[test]
fn a_plain_y_then_enter_long_after_the_question_goes_up_reach_the_pane_and_write_nothing() {
    let sb = Sandbox::new();
    let (mut app, mut s) = asking(&sb);
    // nothing reads a clock to decide; this only rules out a wait
    app.notices.install.up_since = Some(Instant::now() - Duration::from_secs(2));
    key(&mut app, LogicalKey::Char('y'));
    key(&mut app, LogicalKey::Named(NamedKey::Enter));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 4, "the y and the Enter, pressed and let go, reached the pane");
    key(&mut app, LogicalKey::Char('n'));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "and so does an n");
    assert_eq!(app.notices.install.shown_cli(), Some("codex"), "still asking");
    unchanged();
}

#[test]
fn esc_reaches_the_pane_and_leaves_the_question_up() {
    let sb = Sandbox::new();
    let (mut app, mut s) = asking(&sb);
    key(&mut app, LogicalKey::Named(NamedKey::Escape));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2);
    assert_eq!(app.notices.install.shown_cli(), Some("codex"));
    unchanged();
}

#[test]
fn ctrl_option_y_writes_yes_and_takes_the_question_down() {
    let sb = Sandbox::new();
    let (mut app, mut s) = asking(&sb);
    key_with(&mut app, LogicalKey::Char('y'), CHORD);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "the chord and its release were taken");
    assert_eq!(app.notices.install.shown(), None);
    assert!(written().contains("agents.install.codex = yes"), "{}", written());
    key_with(&mut app, LogicalKey::Char('y'), CHORD);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "with nothing asked the chord is the pane's");
}

#[test]
fn ctrl_option_n_writes_no_and_takes_the_question_down() {
    let sb = Sandbox::new();
    let (mut app, mut s) = asking(&sb);
    key_with(&mut app, LogicalKey::Char('n'), CHORD);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0);
    assert_eq!(app.notices.install.shown(), None);
    assert!(written().contains("agents.install.codex = no"), "{}", written());
}

#[test]
fn an_input_method_composing_n_is_left_alone() {
    let sb = Sandbox::new();
    let (mut app, mut s) = asking(&sb);
    app.preedit(0, "n".into());
    assert_eq!(app.windows[0].ime_preedit, "n", "the composition is shown as usual");
    app.preedit(0, "\u{FF4E}".into());
    assert_eq!(app.windows[0].ime_preedit, "\u{FF4E}");
    app.preedit(0, String::new());
    let commit = MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Other, text: Some("\u{FF4E}".into()), ..Default::default() };
    app.key(0, commit, Modifiers::default());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 1, "its commit reached the pane");
    assert_eq!(app.notices.install.shown_cli(), Some("codex"));
    unchanged();
}
