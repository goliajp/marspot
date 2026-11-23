use super::*;
use crate::control::ErrorCode;
use crate::control::response::Reject;
use crate::json::obj;

fn p(args: &str) -> Result<Op, String> {
    parse(&args.split(' ').map(String::from).collect::<Vec<_>>())
}

#[test]
fn pane_control_cli_reads_every_subcommand() {
    assert_eq!(p("list"), Ok(Op::List));
    assert_eq!(
        p("open --visible --cwd /w --size 80x24 -- make -j 4"),
        Ok(Op::Open(OpenArgs { cwd: Some("/w".into()), shadow: false, parent: None, cols: 80, rows: 24, command: Some("make -j 4".into()) }))
    );
    assert_eq!(
        p("send spg --no-enter hello there"),
        Ok(Op::Send(SendArgs { target: "spg".into(), input: Input::Text { text: "hello there".into(), enter: false }, force: false }))
    );
    assert_eq!(
        p("keys 12 --force ctrl-c enter"),
        Ok(Op::Send(SendArgs { target: "12".into(), input: Input::Keys(vec!["ctrl-c".into(), "enter".into()]), force: true }))
    );
    assert_eq!(
        p("run spg --timeout 9 cargo test -p x"),
        Ok(Op::Run(RunArgs { target: "spg".into(), command: "cargo test -p x".into(), timeout_s: 9, force: false }))
    );
    assert_eq!(
        p("read spg -n 40 --wait-text done --since 12"),
        Ok(Op::Read(ReadArgs { target: "spg".into(), history_lines: 40, wait: Some(Wait::Text("done".into())), since: Some(12), timeout_s: 110 }))
    );
    assert_eq!(p("resize 3 100x30"), Ok(Op::Resize { target: "3".into(), cols: 100, rows: 30 }));
    assert_eq!(p("place 3 peek"), Ok(Op::Place { target: "3".into(), place: Where::Peek }));
    assert_eq!(p("close 3"), Ok(Op::Close { target: "3".into() }));
    assert_eq!(p("audit -n 5"), Ok(Op::Audit { limit: 5 }));
}

#[test]
fn pane_control_cli_usage_mistakes() {
    for bad in ["", "fly", "send spg", "keys spg", "run spg", "read", "read a b", "read a --wait-change --wait-exit", "resize 3 big", "place 3 moon", "read a -n"] {
        assert!(p(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn pane_control_cli_exit_codes() {
    let run = p("run a true").unwrap();
    assert_eq!(cli_out::exit_code(&run, &Response::Ok(obj([("exit_status", 3i64.into())]))), 3);
    assert_eq!(cli_out::exit_code(&run, &Response::Ok(obj([("reason", "timeout".into())]))), 1);
    assert_eq!(cli_out::exit_code(&Op::List, &Response::Ok(obj([]))), 0);
    assert_eq!(cli_out::exit_code(&Op::List, &Response::Err(Reject::new(ErrorCode::Disabled, "off"))), 1);
}
