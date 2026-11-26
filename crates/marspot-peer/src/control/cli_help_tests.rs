use super::*;
use crate::control::request::helper::{AgentKind, HelperStop};
use crate::control::{Input, Op, OpenArgs, SendArgs};
use crate::control::{cli, cli_helper, cli_proc};

fn w(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// A whole command line as the binary sees it, down to the op it would
/// send; `Err` means nothing is sent.
fn decide(line: &str) -> Result<Option<(Op, bool)>, Stop> {
    match parse(&w(line))? {
        Top::Control("pane", rest) => cli::parse_cmd(&rest).map(Some),
        Top::Control("helper", rest) => cli_helper::parse_cmd(&rest).map(Some),
        Top::Control(_, rest) => cli_proc::parse_cmd(&rest).map(Some),
        _ => Ok(None),
    }
}

/// Every command and subcommand, with the words it needs before a flag.
const LINES: &[&str] = &[
    "list",
    "whoami",
    "send",
    "mcp",
    "relay a b",
    "claude-hook",
    "gemini-hook",
    "kimi-hook",
    "pane",
    "pane list",
    "pane open",
    "pane send spg",
    "pane keys spg",
    "pane run spg",
    "pane read spg",
    "pane resize spg",
    "pane place spg",
    "pane close spg",
    "pane audit",
    "helper",
    "helper start --agent codex",
    "helper list",
    "helper stop",
    "procs",
    "procs stop",
];

/// Lines whose words after the target are text: a help word there is a
/// mistake, tested below.
const TEXT_AFTER_TARGET: &[&str] = &["pane send spg", "pane run spg"];

#[test]
fn cli_help_and_unknown_options_send_nothing() {
    for line in LINES {
        for help in ["--help", "-h"].iter().filter(|_| !TEXT_AFTER_TARGET.contains(line)) {
            assert_eq!(decide(&format!("{line} {help}")), Err(Stop::Help), "{line} {help}");
            assert_eq!(decide(&format!("{line} --bogus {help}")), Err(Stop::Help), "{line} --bogus {help}");
        }
        // mcp serves past what it does not know: its own test below
        for bad in ["--bogus", "-x", "--help-me"].iter().filter(|_| *line != "mcp") {
            match decide(&format!("{line} {bad}")) {
                Err(Stop::Usage(m)) => assert!(m.contains(bad), "{line} {bad}: {m:?}"),
                other => panic!("{line} {bad}: {other:?}"),
            }
        }
    }
    for help in ["--help", "-h"] {
        assert_eq!(decide(help), Err(Stop::Help));
        // the help word right after the subcommand, before what it needs
        assert_eq!(decide(&format!("pane open {help} --size 1x1")), Err(Stop::Help));
        assert_eq!(decide(&format!("helper start {help} --agent codex task")), Err(Stop::Help));
    }
    assert_eq!(decide(""), Err(Stop::Usage(String::new())));
    assert!(matches!(decide("--bogus"), Err(Stop::Usage(m)) if m.contains("--bogus")));
    assert!(matches!(decide("pane fly"), Err(Stop::Usage(m)) if m.contains("fly")));
    assert!(matches!(decide("pane open --size"), Err(Stop::Usage(m)) if m.contains("needs a value")));
    assert!(matches!(decide("pane open stray"), Err(Stop::Usage(_))));
    assert!(matches!(decide("list extra"), Err(Stop::Usage(_))));
    assert!(matches!(decide("claude-hook extra"), Err(Stop::Usage(_))));
}

#[test]
fn cli_help_words_after_double_dash_pass_through() {
    let open = |c: &str| Ok(Some((Op::Open(OpenArgs { cwd: None, shadow: true, parent: None, cols: 120, rows: 40, command: Some(c.into()) }), false)));
    assert_eq!(decide("pane open -- cmd --help"), open("cmd --help"));
    assert_eq!(decide("pane open --json -- jq -h --json").map(|o| o.map(|(_, j)| j)), Ok(Some(true)));
    let text = |t: &str| Ok(Some((Op::Send(SendArgs { target: "spg".into(), input: Input::Text { text: t.into(), enter: true }, force: false }), false)));
    assert_eq!(decide("pane send spg -- -x"), text("-x"));
    assert_eq!(decide("pane send spg -- --help"), text("--help"));
    // a `-` after `--` is the text, not stdin
    assert_eq!(decide("pane send spg -- -"), text("-"));
    assert_eq!(decide("pane send spg hi -- -x"), text("hi -- -x"));
    assert_eq!(decide("pane send -- spg -x"), text("-x"));
    let keys = decide("pane keys spg -- -x").unwrap().unwrap().0;
    assert_eq!(keys, Op::Send(SendArgs { target: "spg".into(), input: Input::Keys(vec!["-x".into()]), force: false }));
    let run = |line: &str| match decide(line) {
        Ok(Some((Op::Run(r), json))) => (r.command, r.timeout_s, json),
        other => panic!("{line}: {other:?}"),
    };
    assert_eq!(run("pane run spg ls --help"), ("ls --help".into(), 110, false));
    assert_eq!(run("pane run spg ls -la --json"), ("ls -la --json".into(), 110, false));
    assert_eq!(run("pane run spg --timeout 5 -- -weird -h"), ("-weird -h".into(), 5, false));
    assert_eq!(run("pane run --json spg --timeout 5 ls -- x"), ("ls -- x".into(), 5, true));
    match decide("pane read spg --wait-text --help") {
        Ok(Some((Op::Read(r), _))) => assert_eq!(r.wait, Some(crate::control::Wait::Text("--help".into()))),
        other => panic!("{other:?}"),
    }
    match decide("helper start --agent codex --mode --yolo -- -x fix it") {
        Ok(Some((Op::HelperStart(a), _))) => assert_eq!((a.mode.as_deref(), a.task.as_str()), (Some("--yolo"), "-x fix it")),
        other => panic!("{other:?}"),
    }
    assert_eq!(decide("helper stop -- -odd").unwrap().unwrap().0, Op::HelperStop(HelperStop::Name("-odd".into())));
    match decide("procs stop --force -- 12@34") {
        Ok(Some((Op::ProcStop(a), _))) => assert_eq!((a.ids, a.force), (vec!["12@34".to_string()], true)),
        other => panic!("{other:?}"),
    }
    let send = parse(&w("send --now bob -- -x --help")).unwrap();
    assert_eq!(send, Top::Send { now: true, to: "bob".into(), words: w("-x --help"), stdin: false });
    assert_eq!(parse(&w("send bob -")).unwrap(), Top::Send { now: false, to: "bob".into(), words: w("-"), stdin: true });
    assert_eq!(parse(&w("mcp --client-timeout-s 600")).unwrap(), Top::Mcp { client_timeout_s: Some("600".into()), ignored: vec![] });
    assert_eq!(parse(&w("relay a b c")).unwrap(), Top::Relay { public: "a".into(), upstream: "b".into(), send: "c".into() });
}

#[test]
fn cli_help_text_after_its_first_word_is_never_an_option() {
    let send = |line: &str| match parse(&w(line)) {
        Ok(Top::Send { now, to, words, stdin: false }) => (now, to, words.join(" ")),
        other => panic!("{line}: {other:?}"),
    };
    assert_eq!(send("send bob hi --help"), (false, "bob".into(), "hi --help".into()));
    assert_eq!(send("send bob --help me"), (false, "bob".into(), "--help me".into()));
    assert_eq!(send("send bob use --now please"), (false, "bob".into(), "use --now please".into()));
    assert_eq!(send("send --now bob -x"), (true, "bob".into(), "-x".into()));
    assert_eq!(parse(&w("send --help bob hi")), Err(Stop::Help));
    let text = |t: &str, enter| Ok(Some((Op::Send(SendArgs { target: "spg".into(), input: Input::Text { text: t.into(), enter }, force: false }), false)));
    assert_eq!(decide("pane send spg git log -5"), text("git log -5", true));
    assert_eq!(decide("pane send spg say --help --no-enter"), text("say --help --no-enter", true));
    assert_eq!(decide("pane send spg --no-enter git log -5"), text("git log -5", false));
    assert_eq!(decide("pane send --help spg hi"), Err(Stop::Help));
    for line in TEXT_AFTER_TARGET {
        for help in ["--help", "-h"] {
            match decide(&format!("{line} {help}")) {
                Err(Stop::Usage(m)) => assert!(m.contains(help), "{line} {help}: {m:?}"),
                other => panic!("{line} {help}: {other:?}"),
            }
        }
    }
    assert_eq!(decide("pane send spg -- -h"), text("-h", true));
    match decide("helper start --agent codex fix it --help --agent claude") {
        Ok(Some((Op::HelperStart(a), _))) => assert_eq!((a.agent, a.task.as_str()), (AgentKind::Codex, "fix it --help --agent claude")),
        other => panic!("{other:?}"),
    }
    assert_eq!(decide("helper start --help --agent codex fix"), Err(Stop::Help));
}

#[test]
fn cli_help_mcp_keeps_serving_past_words_it_does_not_know() {
    let mcp = |line: &str| parse(&w(line)).unwrap();
    assert_eq!(mcp("mcp -v"), Top::Mcp { client_timeout_s: None, ignored: w("-v") });
    assert_eq!(mcp("mcp -v --client-timeout-s 9 x"), Top::Mcp { client_timeout_s: Some("9".into()), ignored: w("-v x") });
    assert_eq!(mcp("mcp --client-timeout-s"), Top::Mcp { client_timeout_s: None, ignored: w("--client-timeout-s") });
    assert_eq!(parse(&w("mcp --help")), Err(Stop::Help));
}
