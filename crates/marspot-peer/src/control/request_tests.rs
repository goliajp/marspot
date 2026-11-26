use super::*;

fn code_of(line: &str) -> ErrorCode {
    parse(line).unwrap_err().code
}

#[test]
fn pane_control_unknown_keys_are_ignored() {
    let r = parse(r#"{"v":7,"op":"pane.read","later":{"x":1},"caller":{"thread":"t1","who":2},"args":{"target":"spg","new_arg":true}}"#)
        .unwrap();
    assert_eq!(r.caller_thread.as_deref(), Some("t1"));
    let Op::Read(a) = r.op else { panic!() };
    assert_eq!((a.target.as_str(), a.history_lines, a.timeout_s, a.wait, a.since), ("spg", 0, 60, None, None));
}

#[test]
fn pane_control_missing_required_is_bad_request() {
    for line in [
        r#"{}"#,
        r#"[1]"#,
        r#"not json"#,
        r#"{"op":"pane.send","args":{"text":"x"}}"#,
        r#"{"op":"pane.send","args":{"target":"a"}}"#,
        r#"{"op":"pane.send","args":{"target":"a","text":"x","keys":["enter"]}}"#,
        r#"{"op":"pane.run","args":{"target":"a"}}"#,
        r#"{"op":"pane.resize","args":{"target":"a","cols":80}}"#,
        r#"{"op":"pane.place","args":{"target":"a","where":"moon"}}"#,
        r#"{"op":"pane.read","args":{"target":"a","wait":{"quiet_ms":5}}}"#,
        r#"{"op":"pane.read","args":{"target":"a","wait":{"nothing":true}}}"#,
        r#"{"op":"pane.send","args":{"target":"a","keys":["hyper"]}}"#,
        r#"{"op":"pane.send","args":{"target":"a","bytes_b64":"!!"}}"#,
        r#"{"op":"pane.fly"}"#,
    ] {
        assert_eq!(code_of(line), ErrorCode::BadRequest, "{line}");
    }
}

#[test]
fn pane_control_send_over_32k_is_limit_bytes() {
    let big = "x".repeat(32 * 1024 + 1);
    let line = format!(r#"{{"op":"pane.send","args":{{"target":"a","text":"{big}"}}}}"#);
    assert_eq!(code_of(&line), ErrorCode::LimitBytes);
    let run = format!(r#"{{"op":"pane.run","args":{{"target":"a","command":"{big}"}}}}"#);
    assert_eq!(code_of(&run), ErrorCode::LimitBytes);
    let fits = "x".repeat(32 * 1024);
    let run = format!(r#"{{"op":"pane.run","args":{{"target":"a","command":"{fits}"}}}}"#);
    assert!(parse(&run).is_ok());
}

#[test]
fn pane_control_limits_are_clamped() {
    let r = parse(r#"{"op":"pane.read","args":{"target":5,"history_lines":99999,"timeout_s":99999}}"#).unwrap();
    let Op::Read(a) = r.op else { panic!() };
    assert_eq!((a.target.as_str(), a.history_lines, a.timeout_s), ("5", 2000, 3600));
}

#[test]
fn pane_control_every_request_round_trips() {
    let reqs = [
        Op::List,
        Op::Open(OpenArgs { cwd: Some("/w".into()), shadow: false, new_window: false, parent: Some("self".into()), cols: 90, rows: 30, command: Some("ls".into()) }),
        Op::Open(OpenArgs { cwd: None, shadow: false, new_window: true, parent: None, cols: 120, rows: 40, command: None }),
        Op::Send(SendArgs { target: "spg#1".into(), input: Input::Text { text: "a\nb".into(), enter: false }, force: true }),
        Op::Send(SendArgs { target: "1".into(), input: Input::Keys(vec!["ctrl-c".into(), "up".into()]), force: false }),
        Op::Send(SendArgs { target: "1".into(), input: Input::Bytes(vec![0, 27, 255]), force: false }),
        Op::Run(RunArgs { target: "w(1,1,1)".into(), command: "true".into(), timeout_s: 9, force: false }),
        Op::Read(ReadArgs { target: "x".into(), history_lines: 3, wait: Some(Wait::Text("ok".into())), since: Some(77), timeout_s: 5 }),
        Op::Read(ReadArgs { target: "x".into(), history_lines: 0, wait: Some(Wait::Quiet(500)), since: None, timeout_s: 60 }),
        Op::Read(ReadArgs { target: "x".into(), history_lines: 0, wait: Some(Wait::CommandEnd), since: None, timeout_s: 60 }),
        Op::Resize { target: "x".into(), cols: 100, rows: 20 },
        Op::Place { target: "x".into(), place: Where::Peek, force: true },
        Op::Close { target: "x".into() },
        Op::Audit { limit: 7 },
        Op::HelperList { all: true },
        Op::HelperStop(HelperStop::All),
        Op::HelperStop(HelperStop::Name("spg#5521".into())),
        Op::HelperStart(HelperStartArgs {
            agent: AgentKind::Codex,
            task: "fix \"it\"\n$(x)".into(),
            cwd: Some("/w".into()),
            parent: Some("self".into()),
            model: Some("gpt-5".into()),
            profile: Some(3),
            isolation: Isolation::Host,
            mode: Some("ask".into()),
            resume_last: true,
            budget_minutes: 45,
            name: Some("reviewer".into()),
            title: Some("review the parser".into()),
        }),
    ];
    for op in reqs {
        let r = Request { op, caller_thread: Some("th".into()) };
        assert_eq!(parse(&r.to_json()).unwrap(), r);
    }
}

#[test]
fn pane_control_only_reads_are_retried() {
    assert!(Op::List.is_read_only());
    assert!(Op::Audit { limit: 1 }.is_read_only());
    assert!(Op::HelperList { all: false }.is_read_only());
    assert!(!Op::Close { target: "x".into() }.is_read_only());
    assert!(!Op::HelperStop(HelperStop::All).is_read_only());
}

#[test]
fn helpers_start_defaults_and_refusals() {
    let r = parse(r#"{"op":"helper.start","args":{"agent":"kimi","task":"t","budget_minutes":9999,"profile":"P2"}}"#).unwrap();
    let Op::HelperStart(a) = r.op else { panic!() };
    assert_eq!((a.agent, a.isolation, a.budget_minutes, a.profile, a.resume_last), (AgentKind::Kimi, Isolation::Host, 360, Some(2), false));
    let r = parse(r#"{"op":"helper.start","args":{"agent":"claude","task":"t"}}"#).unwrap();
    let Op::HelperStart(a) = r.op else { panic!() };
    assert_eq!(a.budget_minutes, 30);
    for line in [
        r#"{"op":"helper.start","args":{"task":"t"}}"#,
        r#"{"op":"helper.start","args":{"agent":"gpt","task":"t"}}"#,
        r#"{"op":"helper.start","args":{"agent":"codex","task":"  "}}"#,
        r#"{"op":"helper.start","args":{"agent":"codex","task":"t","isolation":"vm"}}"#,
        r#"{"op":"helper.start","args":{"agent":"codex","task":"t","resume":"first"}}"#,
        r#"{"op":"helper.stop","args":{}}"#,
        r#"{"op":"helper.fly"}"#,
    ] {
        assert_eq!(code_of(line), ErrorCode::BadRequest, "{line}");
    }
    let big = "x".repeat(64 * 1024 + 1);
    let line = format!(r#"{{"op":"helper.start","args":{{"agent":"codex","task":"{big}"}}}}"#);
    assert_eq!(code_of(&line), ErrorCode::LimitBytes);
}

#[test]
fn pane_control_open_in_a_new_window() {
    let r = parse(r#"{"op":"pane.open","args":{"new_window":true}}"#).unwrap();
    let Op::Open(a) = r.op else { panic!() };
    assert!(a.new_window && !a.shadow && a.parent.is_none(), "a new window means no shadow by default");
    for bad in [r#"{"new_window":true,"shadow":true}"#, r#"{"new_window":true,"parent":"self"}"#] {
        let line = format!(r#"{{"op":"pane.open","args":{bad}}}"#);
        assert_eq!(parse(&line).unwrap_err().code, ErrorCode::BadRequest, "{bad}");
    }
    let Op::Open(a) = parse(r#"{"op":"pane.open","args":{}}"#).unwrap().op else { panic!() };
    assert!(!a.new_window && a.shadow, "the default stays a shadow pane");
}
