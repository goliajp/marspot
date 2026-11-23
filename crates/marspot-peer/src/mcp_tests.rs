use super::*;

fn server() -> Server {
    Server::at(std::env::temp_dir().join("mcp-no-such-app.sock"), None).waiting(std::time::Duration::from_millis(50))
}

fn names(r: &Value) -> Vec<String> {
    r.at(&["result", "tools"]).unwrap().arr().iter().filter_map(|t| t.str_at("name")).map(String::from).collect()
}

#[test]
fn initialize_and_tool_listing() {
    let mut s = server();
    let r = s.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"codex-mcp-client"}}}"#, 0).unwrap();
    assert_eq!(r.at(&["result", "protocolVersion"]).and_then(Value::str), Some("2025-06-18"));
    assert!(s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, 0).is_none());
    let r = s.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#, 0).unwrap();
    assert_eq!(names(&r), ["ListAgents", "SendMessage", "Panes", "PaneOpen", "PaneSend", "PaneRun", "PaneRead", "PaneSet", "HelperStart", "Helpers", "HelperStop"]);
    for t in &r.at(&["result", "tools"]).unwrap().arr()[..2] {
        assert!(t.str_at("description").unwrap().contains("`name#code`") && t.str_at("description").unwrap().contains("`kind:name`"));
    }
    let r = s.handle(r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#, 0).unwrap();
    assert_eq!(r.at(&["error", "code"]).and_then(Value::i64), Some(-32601));
}

/// Claude Code has its own ListAgents and SendMessage.
#[test]
fn mcp_a_claude_client_gets_only_the_pane_tools() {
    let mut s = server();
    s.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":"claude-code","version":"2"}}}"#, 0);
    let r = s.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, 0).unwrap();
    assert_eq!(names(&r), ["Panes", "PaneOpen", "PaneSend", "PaneRun", "PaneRead", "PaneSet", "HelperStart", "Helpers", "HelperStop"]);
    // the read wait's cap is in the schema the client sees
    let read = r.at(&["result", "tools"]).unwrap().arr().iter().find(|t| t.str_at("name") == Some("PaneRead")).unwrap();
    assert_eq!(read.at(&["inputSchema", "properties", "timeout_s", "maximum"]).and_then(Value::i64), Some(1795));
}

/// A send from a thread marspot has no record for names the reason
/// and sends nothing.
#[test]
fn a_send_from_an_unknown_thread_is_an_error() {
    let mut s = server();
    let line = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"SendMessage","arguments":{"to":"x","message":"hi"},"_meta":{"threadId":"00000000-0000-0000-0000-000000000000"}}}"#;
    let r = s.handle(line, 0).unwrap();
    assert_eq!(r.at(&["result", "isError"]).and_then(Value::bool), Some(true));
    let text = r.at(&["result", "content"]).unwrap().arr()[0].str_at("text").unwrap();
    assert!(text.contains("has no inbox"), "{text}");
    let no_meta = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"SendMessage","arguments":{"to":"x","message":"hi"}}}"#;
    let r = s.handle(no_meta, 0).unwrap();
    assert_eq!(r.at(&["result", "isError"]).and_then(Value::bool), Some(true));
}
