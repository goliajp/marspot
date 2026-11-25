//! Screens as gemini draws them: its quota dialog, its tool approval
//! list and its input box, under a footer row.

use super::*;

const FOOTER: &str = "~/w/a (main)        no sandbox        gemini-3-pro-preview (98% context left)";

fn input(typed: &str) -> String {
    format!("╭──────────────────────────────────────────────╮\n│ > {typed:<44} │\n╰──────────────────────────────────────────────╯\n{FOOTER}")
}

#[test]
fn gemini_screen_a_limit_line_in_the_history_is_not_the_dialog() {
    let dialog = "│ Usage limit reached for gemini-2.5-pro.     │\n│ ● 1. Switch to gemini-2.5-flash             │\n│   2. Stop                                   │\n╰─────────────────────────────────────────────╯\n";
    assert!(bottom_dialog(&(dialog.to_string() + FOOTER)).is_some());
    // an old error line, the prompt back under it
    let old = format!("✕ Usage limit reached for gemini-2.5-pro. Access resets at 4:30 PM PST.\n\n> next question\n✦ an answer\n{}", input(""));
    assert_eq!(bottom_dialog(&old), None);
    // the dialog with its options scrolled into the history
    let scrolled = format!("{dialog}\n> later\n{}", input(""));
    assert_eq!(bottom_dialog(&scrolled), None);
    // a limit line without any option rows under it
    assert_eq!(bottom_dialog(&format!("│ Usage limit reached for all Pro models. │\n{FOOTER}")), None);
}

#[test]
fn gemini_screen_the_approval_list_is_seen_at_the_bottom_only() {
    let asking = format!("│ ? Shell cargo test │\n│ Allow execution of: 'cargo'? │\n│ ● 1. Allow once │\n│   2. Allow for this session │\n│   3. No, suggest changes (esc) │\n{FOOTER}");
    assert!(asks_approval(&asking));
    let answered = format!("{asking}\n{}", "✦ done\n".repeat(30) + &input(""));
    assert!(!asks_approval(&answered), "scrolled away long ago");
    assert!(!asks_approval(&input("")));
}

#[test]
fn gemini_screen_typed_text_is_told_from_the_placeholder() {
    assert!(!has_typed(&input("")));
    assert!(!has_typed(&input("  Type your message or @path/to/file")));
    assert!(has_typed(&input("fix the parser")));
    // a long entry whose first row is blank
    let long = format!("╭────╮\n│ >        │\n│   second row │\n╰────╯\n{FOOTER}");
    assert!(has_typed(&long));
    // shell mode
    assert!(has_typed(&input("").replace("│ > ", "│ ! ").replace("│ !  ", "│ ! ls")));
    assert!(!has_typed(FOOTER), "no input box on screen");
}
