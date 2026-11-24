use super::{SCREEN_LINES, prompt_of, prompt_sha};

/// codex 0.172's approval prompt, as its screen reads, below the turn
/// that asked for it.
const CODEX_ASKS: &str = "  Would you like to run the following command?

  Environment: local

  Reason: Allow the same command to reach example.com? DNS resolution failed inside the sandbox.

  $ curl -sI https://example.com


› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `curl -sI https://example.com` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
";

fn codex_screen(turn: &str, blank_rows: usize) -> String {
    let head = "\n  >_ OpenAI Codex (v0.172.0)\n     ~/work/demo\n\n\n› Run exactly this shell command and nothing else: curl -sI https://example.com\n\n\n";
    format!("{head}{turn}{}{CODEX_ASKS}", "\n".repeat(blank_rows))
}

/// codex draws no rule above its approval prompt: the prompt starts at
/// its question, whatever the turn above it shows.
#[test]
fn helpers_a_codex_approval_prompt_starts_at_its_question() {
    let turn = "• I’ll run the command as written.\n\n• Exited (status 6) curl -sI https://example.com\n  └ (no output)\n\n• Running curl -sI https://example.com\n";
    let screen = codex_screen(turn, 12);
    let prompt = prompt_of(&screen);
    assert!(prompt.starts_with("  Would you like to run the following command?"), "{prompt}");
    assert!(prompt.ends_with("3. No, and tell Codex what to do differently (esc)"), "{prompt}");
    // the turn above grows into the last rows, or reads otherwise
    let close = codex_screen(turn, 1);
    let other = codex_screen("• Thinking it over\n", 1);
    assert_eq!(prompt_sha(&screen), prompt_sha(&close));
    assert_eq!(prompt_sha(&screen), prompt_sha(&other));
    // nor where the cursor is
    let moved = screen.replace("› 1. Yes, proceed", "  1. Yes, proceed").replace("  2. Yes, and don't", "› 2. Yes, and don't");
    assert_eq!(prompt_sha(&screen), prompt_sha(&moved));
    // another question is another prompt
    assert_ne!(prompt_sha(&screen), prompt_sha(&screen.replace("$ curl -sI", "$ curl -sL")));
}

/// A rule lower than codex's question still marks where a prompt starts.
#[test]
fn helpers_a_rule_below_the_question_wins() {
    let screen = format!("  Would you like to run it?\n{}\nRun make?\n❯ 1. Yes\n  2. No\n", "─".repeat(20));
    assert_eq!(prompt_of(&screen), "Run make?\n❯ 1. Yes\n  2. No");
}

/// A rule marks where a prompt starts wherever codex's question is.
#[test]
fn helpers_a_rule_above_the_question_wins_too() {
    let screen = format!("{}\nRun make?\n  Would you like to run it?\n❯ 1. Yes\n  2. No\n", "─".repeat(20));
    assert_eq!(prompt_of(&screen), "Run make?\n  Would you like to run it?\n❯ 1. Yes\n  2. No");
}

/// codex's question far up the transcript, or its words in a line that
/// asks nothing, are not where a prompt drawn below with no rule starts.
#[test]
fn helpers_a_question_far_up_or_asking_nothing_is_not_codexs() {
    let bottom = "Run make?\n❯ 1. Yes\n  2. No";
    let question = CODEX_ASKS.lines().next().unwrap();
    let far = format!("{question}\n{}{bottom}\n", "• it printed a line\n".repeat(SCREEN_LINES));
    let rows: Vec<&str> = far.lines().collect();
    assert_eq!(prompt_of(&far), rows[rows.len() - SCREEN_LINES..].join("\n"), "the rows that end at the choice");
    let near = format!("• Done.\n  Would you like to run the following command, it said\n{bottom}\n");
    assert_eq!(prompt_of(&near), near.trim_end(), "no question asked");
}
