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
    assert!(expired::judge(now - 1, Some(now + 1), refusal, now).unwrap().renewed);
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
