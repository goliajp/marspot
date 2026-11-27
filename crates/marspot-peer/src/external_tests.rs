use super::*;
use crate::envelope;
use crate::registry::Agent;

fn entry(pid: i32, name: &str) -> Entry {
    Entry { pid, name: Some(name.into()), socket: Some(format!("/tmp/cc-socks/{pid}.sock")), class: None, ..sender("x") }
}

fn pane(base: &str, code: u16, shown: &str, pid: i32) -> Pane {
    Pane { name: shown.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock"), code: Some(code), base: Some(base.into()), agent: Some("claude".into()), shadow: false }
}

#[test]
fn external_names_keep_to_a_header_safe_charset() {
    for ok in ["devops-cron", "a", "job_7.daily", &"x".repeat(MAX_NAME)] {
        assert_eq!(check_name(ok), Ok(()), "{ok}");
    }
    let long = "x".repeat(MAX_NAME + 1);
    for bad in ["", &long, "-x", ".x", "external:x", "a b", "a\"b", "a<b>", "a\nfrom-name=\"x\"", "a\u{7}", "a\u{200B}b", "名前", "a#1024"] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
}

/// Every form a pane answers to, and a session's own name, is refused
/// as an outside sender's name, in any case.
#[test]
fn external_names_a_pane_or_session_has_are_taken() {
    let es = vec![entry(1, "marspot-6a"), entry(2, "gds")];
    let panes = vec![pane("marspot", 1024, "marspot#1024", 1), pane("alt", 3381, "alt", 3)];
    for name in ["marspot", "MARSPOT#1024", "marspot#1024", "alt", "alt#3381", "marspot-6a", "GDS"] {
        assert!(taken(name, &es, &panes).is_some(), "{name}");
    }
    assert_eq!(taken("alt#3381", &es, &panes).as_deref(), Some("pane alt#3381"));
    assert_eq!(taken("gds", &es, &panes).as_deref(), Some("session gds (pid 2)"));
    assert_eq!(taken("devops-cron", &es, &panes), None);
}

#[test]
fn external_is_refused_inside_any_session() {
    let codex = Entry { ours: true, agent: Some(Agent::Codex), agent_pid: Some(30), ..entry(900, "proj#9906") };
    let es = vec![entry(10, "claude-6a"), codex];
    let none = |_: &str| false;
    // 50 (this) → 40 (shell) → 30 (codex) → 20 (zsh) → 10 (claude) → 1
    let table = |pid: i32| [(50, 40), (40, 30), (30, 20), (20, 10), (10, 1), (70, 60), (60, 1)].iter().find(|(p, _)| *p == pid).map(|(_, pp)| *pp);
    assert_eq!(session_around(&es, 50, table, none).as_deref(), Some("the codex session proj#9906 (pid 30)"));
    assert_eq!(session_around(&es, 20, table, none).as_deref(), Some("the claude session claude-6a (pid 10)"));
    assert_eq!(session_around(&es, 70, table, none), None, "a cron job under launchd is outside");
    let set = |k: &'static str| move |v: &str| v == k;
    assert!(session_around(&es, 70, table, set("CODEX_THREAD_ID")).unwrap().contains("CODEX_THREAD_ID"));
    assert!(session_around(&es, 70, table, set("CLAUDECODE")).unwrap().contains("CLAUDECODE"));
    let err = send("devops-cron", Some("x".into()), &es, &[], "gds", "hi", Priority::Next).err().unwrap();
    assert!(err.starts_with("--from is only for senders outside every agent session, and this runs inside x; nothing was sent"), "{err}");
}

/// The header the recipient reads names the sender as `external:` and
/// nothing else, with an address and the bypass class.
#[test]
fn external_header_says_external() {
    let me = sender("devops-cron");
    assert_eq!(peers::display_name(&me, &[pane("x", 1, "x", 1)]), "external:devops-cron");
    let from = format!("uds:{}", me.socket.as_deref().unwrap());
    assert!(envelope::is_address(&from));
    let h = envelope::Header { from, from_name: Some(peers::display_name(&me, &[])), from_mode: me.class };
    let p = envelope::parse(&envelope::encode(&h, "report")).unwrap();
    assert_eq!((p.from_name.as_deref(), p.from_mode), (Some("external:devops-cron"), Some(Mode::Bypass)));
}

#[test]
fn external_checks_come_before_anything_is_sent() {
    let es = vec![entry(2, "gds")];
    let err = send("gds", None, &es, &[], "gds", "hi", Priority::Next).err().unwrap();
    assert!(err.starts_with("--from \"gds\" is the name of session gds (pid 2)"), "{err}");
    assert!(send("a b", None, &es, &[], "gds", "hi", Priority::Next).err().unwrap().contains("letters, digits"));
}
