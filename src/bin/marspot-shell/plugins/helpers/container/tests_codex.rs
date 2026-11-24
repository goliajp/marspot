use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use marspot_peer::control::AgentKind;
use marspot_peer::control::b64;
use marspot_peer::json;

use super::super::notify::Stop;
use super::super::table::Event;
use super::super::tests::{shared_at, temp};
use super::tests::{boxed, written};
use super::*;

const ACCESS_SIG: &str = "ACCESS-SIG-0123";
const REFRESH: &str = "REFRESH-SECRET-0123";

/// A JWT, unsigned, that ends at `exp` (Unix seconds).
fn jwt(exp: i64, sig: &str) -> String {
    let url = |s: &str| b64::encode(s.as_bytes()).replace('+', "-").replace('/', "_").trim_end_matches('=').to_string();
    format!("{}.{}.{sig}", url(r#"{"alg":"none"}"#), url(&format!(r#"{{"exp":{exp},"sub":"x?>"}}"#)))
}

/// An account's `auth.json`, refresh token and all, whose access token
/// lasts an hour from now.
fn host_auth() -> String {
    let exp = records::now_ms() as i64 / 1000 + 3600;
    format!(
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"{}","access_token":"{}","refresh_token":"{REFRESH}","account_id":"acct-1"}},"last_refresh":"2026-11-14T22:50:06.038985Z"}}"#,
        jwt(exp - 3000, "ID"),
        jwt(exp, ACCESS_SIG)
    )
}

pub(super) fn auth() -> codex_auth::Auth {
    codex_auth::parse(&host_auth()).unwrap()
}

/// A codex account with a login, and much besides.
pub(super) fn account(home: &Path) {
    std::fs::create_dir_all(home.join("rules")).unwrap();
    std::fs::write(home.join("auth.json"), host_auth()).unwrap();
    std::fs::write(home.join("config.toml"), "model = \"gpt-x\"\nmodel_reasoning_effort = \"high\"\nnotify = [\"evil\"]\n[mcp_servers.m]\ncommand = \"evil\"\n").unwrap();
    std::fs::write(home.join("rules/default.rules"), "allow everything").unwrap();
}

#[test]
fn container_codex_token_ends_where_its_jwt_says() {
    assert_eq!(codex_auth::jwt_exp_ms(&jwt(1_795_561_638, "s")), Some(1_795_561_638_000));
    for bad in ["", "a.b.c", "nodots", "a.!!!.c"] {
        assert_eq!(codex_auth::jwt_exp_ms(bad), None, "{bad}");
    }
    let a = auth();
    assert!(a.expires_at.is_some_and(|ms| ms > records::now_ms() as i64));
    assert!(codex_auth::parse(r#"{"tokens":{"id_token":"x"}}"#).is_none(), "no access token: no login");
    assert!(codex_auth::parse(r#"{"tokens":{"id_token":"x","access_token":"two words"}}"#).is_none());
}

#[test]
fn container_codex_login_has_no_refresh_token() {
    let dir = temp("container-codex-auth");
    account(&dir.join("account"));
    let read = codex_auth::read(&dir.join("account")).unwrap();
    let text = codex_auth::text(&read);
    assert!(!text.contains(REFRESH), "{text}");
    let v = json::parse(&text).unwrap();
    assert_eq!(v.at(&["tokens", "refresh_token"]).and_then(|t| t.str()), Some(""), "empty, not missing: codex takes no login without the key");
    assert!(v.at(&["tokens", "access_token"]).and_then(|t| t.str()).is_some_and(|t| t.ends_with(ACCESS_SIG)));
    assert_eq!(v.at(&["tokens", "account_id"]).and_then(|t| t.str()), Some("acct-1"));
    assert_eq!(v.str_at("last_refresh"), Some("2026-11-14T22:50:06.038985Z"));
    // a link planted where it goes is replaced, not written through
    let (home, outside) = (dir.join("home"), dir.join("outside"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(&outside, "host file").unwrap();
    symlink(&outside, home.join("auth.json")).unwrap();
    symlink(&outside, home.join(".auth.json.marspot")).unwrap();
    codex_auth::write(&home, &read).unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host file");
    let m = std::fs::symlink_metadata(home.join("auth.json")).unwrap();
    assert!(m.is_file() && m.permissions().mode() & 0o777 == 0o600);
    assert_eq!(std::fs::read_to_string(home.join("auth.json")).unwrap(), text);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_gets_its_token_and_nothing_else_of_the_account() {
    let (dir, run) = written(AgentKind::Codex, "auto");
    let d = dir.display();
    assert!(run.contains(&format!("--mount 'type=bind,source={d}/home,target=/home/node/.codex'")), "{run}");
    assert_eq!((run.matches(" -v ").count(), run.matches(" --mount ").count()), (0, 3), "{run}");
    assert!(run.contains(&format!(" codex -m 'a-model' '-a' 'never' '-s' 'danger-full-access' \"$(cat '{d}/task.md')\" </dev/tty &")), "{run}");
    assert!(!run.contains("--env-file") && !dir.join("env").exists());
    let again = std::fs::read_to_string(dir.join("resume.sh")).unwrap();
    assert!(again.contains(" codex resume --last -m 'a-model' '-a' 'never' ") && !again.contains("task.md"), "{again}");
    let auth = std::fs::read_to_string(dir.join("home/auth.json")).unwrap();
    assert!(auth.contains(ACCESS_SIG) && !auth.contains(REFRESH));
    assert_eq!(std::fs::metadata(dir.join("home/auth.json")).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(login::expires_ms(&dir).is_some_and(|ms| ms > records::now_ms() as i64));
    let config = std::fs::read_to_string(dir.join("home/config.toml")).unwrap();
    assert_eq!(config, "check_for_update_on_startup = false\nmodel = \"gpt-x\"\nmodel_reasoning_effort = \"high\"\n");
    assert!(!dir.join("home/rules").exists());
    for f in ["run.sh", "resume.sh", "task.md"] {
        assert!(!std::fs::read_to_string(dir.join(f)).unwrap().contains(ACCESS_SIG), "{f}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_refused_login_is_judged_like_claudes() {
    let refusal = "Error: account/read failed: workspace routing discovery unauthorized (401) (code -32603)";
    let now = 100 * 60_000;
    assert!(expired::refused(refusal) && expired::refused("ERROR: Failed to refresh token: 400 Bad Request"));
    assert!(expired::judge(AgentKind::Codex, now - 1, Some(now + 1), refusal, now).unwrap().renewed);
    let text = login::none_text(AgentKind::Codex, &vec![(1, Some(1500))], 30);
    assert!(text.starts_with("a codex helper in a container needs an account whose login lasts 40 min"), "{text}");
    assert!(text.contains("renewed when codex runs on that account on the host"), "{text}");

    let dir = temp("container-codex-expired");
    let (s, _rx) = shared_at(&dir);
    s.record(41, Event::Started(boxed(AgentKind::Codex)));
    let h = s.table().map[&41].clone();
    std::fs::create_dir_all(s.dir_of(41)).unwrap();
    std::fs::write(s.dir_of(41).join("expires"), "5").unwrap();
    let over = at_stop(&s, &h, Stop::Idle, Some(refusal)).unwrap();
    assert!(over.note.contains("the login in its container expired"), "{}", over.note);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_login_refused_before_its_end_takes_the_newer_one() {
    let refusal = "■ unexpected status 401 Unauthorized: Could not parse your authentication token. Please try signing in again.";
    let now = 100 * 60_000;
    let expires = now + 30 * 60_000;
    assert!(expired::judge(AgentKind::Codex, expires, None, "• Working (3m 10s)", now).is_none(), "not refused, and not over");
    let o = expired::judge(AgentKind::Codex, expires, Some(expires + 1), refusal, now).unwrap();
    assert!(o.renewed && o.note.contains("still had 30 min left") && o.note.contains("started again"), "{}", o.note);
    let o = expired::judge(AgentKind::Codex, expires, Some(expires), refusal, now).unwrap();
    assert!(!o.renewed && o.note.contains("has not been renewed"), "the same login on the host: {}", o.note);
    // before its end only codex's own error line near the bottom counts
    let output = "  └ HTTP/1.1 401 Unauthorized\n    api error: 401 authentication_error\n• The endpoint says 401 Unauthorized without a key.\n\n› Ask\n";
    assert!(expired::judge(AgentKind::Codex, expires, Some(expires + 1), output, now).is_none(), "a command's 401 is not the token's");
    let far = format!("{refusal}\n{}", "• more output\n".repeat(8));
    assert!(expired::judge(AgentKind::Codex, expires, Some(expires + 1), &far, now).is_none(), "an old error further up");
    let near = format!("{refusal}\n\n› Ask\n\n  ? for shortcuts\n");
    assert!(expired::judge(AgentKind::Codex, expires, Some(expires + 1), &near, now).unwrap().renewed);
    assert!(expired::judge(AgentKind::Codex, now - 1, Some(now + 1), output, now).unwrap().renewed, "once it ended any refusal counts");
}

/// The error line as the reply's rows are drawn: indented under it.
#[test]
fn container_codex_error_line_counts_indented() {
    let now = 100 * 60_000;
    let expires = now + 30 * 60_000;
    let screen = "• Working (3m 10s)\n  ■ unexpected status 401 Unauthorized: Could not parse your authentication token.\n\n› Ask\n";
    assert!(expired::judge(AgentKind::Codex, expires, Some(expires + 1), screen, now).unwrap().renewed);
}

/// Stopped while its token was being written: the token goes again.
#[test]
fn container_codex_helper_stopped_while_its_token_is_written_loses_it() {
    let dir = temp("container-codex-again-late");
    let fd = dir.join("fake");
    std::fs::create_dir_all(&fd).unwrap();
    let d = super::tests_away::fake(&fd);
    std::fs::write(fd.join("rm-s"), "1").unwrap();
    account(&dir.join(".codex"));
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("HOME", dir.display().to_string());
    let (s, _rx) = shared_at(&dir.join("helpers"));
    s.record(41, Event::Started(super::super::tests::started(1, "claude", AgentKind::Codex)));
    std::fs::create_dir_all(s.dir_of(41).join("home")).unwrap();
    let me = std::sync::Arc::clone(&s);
    let run = std::thread::spawn(move || expired::again(&me, 41, AgentKind::Codex, "codex", Some(d)));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(fd.join("log")).unwrap_or_default().lines().any(|l| l.starts_with("rm ")) {
        assert!(std::time::Instant::now() < until, "docker rm was not asked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // its stop lands while docker removes the old container
    s.record(41, Event::Stopped { exited: false });
    let e = run.join().unwrap().unwrap_err();
    assert!(e.contains("stopped"), "{e}");
    assert!(s.dir_of(41).join("expires").exists(), "the token was written");
    assert!(!s.dir_of(41).join("home/auth.json").exists(), "and taken away");
    drop(env);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Stopped while its token was being written, and the writing failed
/// part way: what was written goes all the same.
#[test]
fn container_codex_helper_stopped_while_its_token_failed_to_be_written_loses_it() {
    let dir = temp("container-codex-again-failed");
    let fd = dir.join("fake");
    std::fs::create_dir_all(&fd).unwrap();
    let d = super::tests_away::fake(&fd);
    std::fs::write(fd.join("rm-s"), "1").unwrap();
    account(&dir.join(".codex"));
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("HOME", dir.display().to_string());
    let (s, _rx) = shared_at(&dir.join("helpers"));
    s.record(41, Event::Started(super::super::tests::started(1, "claude", AgentKind::Codex)));
    std::fs::create_dir_all(s.dir_of(41).join("home")).unwrap();
    // the token's end cannot be written: a directory is in its place
    std::fs::create_dir_all(s.dir_of(41).join("expires")).unwrap();
    let me = std::sync::Arc::clone(&s);
    let run = std::thread::spawn(move || expired::again(&me, 41, AgentKind::Codex, "codex", Some(d)));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(fd.join("log")).unwrap_or_default().lines().any(|l| l.starts_with("rm ")) {
        assert!(std::time::Instant::now() < until, "docker rm was not asked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    s.record(41, Event::Stopped { exited: false });
    let e = run.join().unwrap().unwrap_err();
    assert!(e.contains("stopped"), "{e}");
    assert!(!s.dir_of(41).join("home/auth.json").exists(), "what was written is taken away");
    drop(env);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_stopped_helper_is_not_started_again() {
    let dir = temp("container-codex-again");
    let fd = dir.join("fake");
    std::fs::create_dir_all(&fd).unwrap();
    let d = super::tests_away::fake(&fd);
    account(&dir.join(".codex"));
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("HOME", dir.display().to_string());
    let (s, _rx) = shared_at(&dir.join("helpers"));
    std::fs::create_dir_all(s.dir_of(41).join("home")).unwrap();
    // no longer in the table: its stop came before the restart ran
    let e = expired::again(&s, 41, AgentKind::Codex, "codex", Some(d)).unwrap_err();
    assert!(e.contains("stopped"), "{e}");
    assert!(!s.dir_of(41).join("home/auth.json").exists() && !s.dir_of(41).join("expires").exists());
    assert!(!std::fs::read_to_string(fd.join("log")).unwrap().lines().any(|l| l.starts_with("rm ")), "docker was not asked");
    drop(env);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_ask_and_own_flags_run_without_its_sandbox() {
    let (dir, run) = written(AgentKind::Codex, "ask");
    assert!(run.contains(" codex -m 'a-model' '-s' 'danger-full-access' \"$(cat "), "{run}");
    assert!(!run.contains("'-a'"), "it asks as codex does by default: {run}");
    std::fs::remove_dir_all(&dir).unwrap();
    let (dir, run) = written(AgentKind::Codex, "-c model_verbosity=low");
    assert!(run.contains(" '-c' 'model_verbosity=low' '-s' 'danger-full-access' "), "{run}");
    std::fs::remove_dir_all(&dir).unwrap();
    for kept in ["--dangerously-bypass-approvals-and-sandbox", "-a on-request --sandbox danger-full-access", "-sdanger-full-access", "-s=danger-full-access"] {
        let (dir, run) = written(AgentKind::Codex, kept);
        assert_eq!(run.matches("danger-full-access").count(), kept.matches("danger-full-access").count(), "{run}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for refused in ["-s workspace-write", "-sworkspace-write", "-s=read-only", "-s", "--sandbox=read-only", "--full-auto", "-c sandbox_mode=\"workspace-write\""] {
        let dir = temp("container-codex-sandbox");
        super::tests::account(AgentKind::Codex, &dir.join("account"));
        let r = write_files(&dir, 41, &super::tests::args(AgentKind::Codex), "proj#12", refused, &super::tests::ready(AgentKind::Codex, &dir.join("account")));
        let e = r.err().unwrap_or_else(|| panic!("{refused} was taken"));
        assert!(e.message.contains("the container is the sandbox"), "{}", e.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn container_codex_login_without_an_end_is_not_handed_over() {
    let dir = temp("container-codex-no-end");
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let a = codex_auth::parse(r#"{"tokens":{"id_token":"x","access_token":"not-a-jwt","refresh_token":"r"}}"#).unwrap();
    assert!(a.expires_at.is_none());
    let e = login::hand_over(&dir, &login::Login::Codex(a)).err().unwrap();
    assert!(e.message.contains("has no end"), "{}", e.message);
    assert!(!dir.join("expires").exists() && !dir.join("home/auth.json").exists());
    // a file that cannot be put in place leaves no part of a token behind
    std::fs::create_dir_all(dir.join("home/auth.json/x")).unwrap();
    assert!(codex_auth::write(&dir.join("home"), &auth()).is_err());
    assert!(!dir.join("home/.auth.json.marspot").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_codex_default_home_is_an_account() {
    let dir = temp("container-codex-default");
    account(&dir.join(".codex"));
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("HOME", dir.display().to_string());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (a, l) = login::pick(AgentKind::Codex, None, None, 30, until).unwrap_or_else(|r| panic!("{}", r.message));
    assert_eq!((a.n, a.home.clone()), (0, dir.join(".codex")));
    assert!(l.expires_at().is_some());
    assert!(login::of_label(AgentKind::Codex, &a.label()).is_some(), "found again by its label");
    drop(env);
    std::fs::remove_dir_all(&dir).unwrap();
}
