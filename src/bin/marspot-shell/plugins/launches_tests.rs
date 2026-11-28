use super::*;

/// A switch's line: the screen clear, the subshell that keeps the
/// directory, the account, and the sentence it carries.
#[test]
fn a_switch_line_names_its_agent_account_and_arguments() {
    let line = "printf '\\033[H\\033[2J'; ( cd '/w/my proj' && CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' exec claude --mcp-config '/s/claude-mcp.json' --resume u-1 --model 'claude-fable-5-1[1m]' --effort high -- 'carry on please' )\r";
    let (agent, account, argv) = describe(line).unwrap();
    assert_eq!(agent, "claude");
    assert_eq!(account, "P3");
    assert_eq!(argv, "claude --mcp-config /s/claude-mcp.json --resume u-1 --model claude-fable-5-1[1m] --effort high -- <prompt>");
}

#[test]
fn other_agents_and_a_helper_script_are_read_too() {
    let codex = describe("CODEX_HOME='/Users/x/.codex-profile-2' codex resume t-9 -m gpt-5.5\r").unwrap();
    assert_eq!(codex, ("codex", "P2".to_string(), "codex resume t-9 -m gpt-5.5".to_string()));
    let helper = describe("exec env claude --model claude-opus-5-5 -- \"$(cat '/s/helpers/7/task.md')\"").unwrap();
    assert_eq!(helper, ("claude", "default".to_string(), "claude --model claude-opus-5-5 -- <prompt>".to_string()));
    let task = describe("exec env CODEX_HOME='/h/.codex' codex \"$(cat '/s/task.md')\"").unwrap();
    assert_eq!(task.2, "codex <task>", "the task file's text never reaches the log");
}

#[test]
fn a_line_that_starts_no_agent_is_no_launch() {
    assert_eq!(describe("ls -la\r"), None);
    assert_eq!(describe("\x03"), None);
    assert_eq!(describe("FOO=bar claudette\r"), None);
}

#[test]
fn reasons_come_from_the_run_name() {
    assert_eq!(reason_of("cc.profile_cycle"), "cycle");
    assert_eq!(reason_of("codex.switch_back"), "cycle");
    assert_eq!(reason_of("cc.reclaim"), "reclaim");
    assert_eq!(reason_of("cc.cold_restore"), "cold_restore");
    assert_eq!(reason_of("handoff.switch"), "handoff");
    assert_eq!(reason_of("kimi.wake"), "kimi.wake");
}

/// The hook claims a line that starts an agent and passes on any other.
#[test]
fn the_hook_claims_agent_lines_only() {
    let hook = after_typed();
    assert!(hook(7, "cc.cold_restore", b"claude --resume u --model m\r").is_some());
    assert!(hook(7, "cc.cold_restore", b"\x03").is_none());
    assert!(hook(7, "cli.send", b"ls\r").is_none());
}

/// An agent marspot announced in a pane is used up by the first sight
/// of it there; anything else in that pane is not marspot's.
#[test]
fn an_announced_launch_is_claimed_once() {
    let l = Launch { reason: "cold_restore", sid: 9_001, agent: "claude", account: "P3".into(), argv: "claude".into() };
    announce(&l);
    assert!(!claimed(9_002, "claude"), "another pane");
    assert!(!claimed(9_001, "codex"), "another agent");
    assert!(claimed(9_001, "claude"));
    assert!(!claimed(9_001, "claude"), "used up");
}
