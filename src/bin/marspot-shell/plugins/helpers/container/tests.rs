use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use marspot_peer::control::{AgentKind, HelperStartArgs, Isolation};

use super::super::notify::{self, Stop};
use super::super::table::{Event, Helper, Started};
use super::super::tests::{shared_at, started, temp};
use super::super::{launch::Account, turn};
use super::*;

const TOKEN: &str = "sk-fake-TOKEN-0123456789";

pub(super) fn args(agent: AgentKind) -> HelperStartArgs {
    HelperStartArgs {
        agent,
        task: "Say 'hi' to $HOME `now`".into(),
        cwd: None,
        parent: None,
        model: Some("a-model".into()),
        profile: None,
        isolation: Isolation::Container,
        mode: None,
        resume_last: false,
        budget_minutes: 30,
        name: None,
    }
}

pub(super) fn ready(agent: AgentKind, home: &Path) -> Ready {
    Ready {
        docker: PathBuf::from("/usr/local/bin/docker"),
        image_there: true,
        account: Account { n: 2, home: home.into() },
        binds: creds::of(agent, home).unwrap(),
        cwd: "/Users/someone/proj".into(),
        login: match agent {
            AgentKind::Claude => Some(login::Login::Claude(login::ClaudeLogin { token: TOKEN.into(), expires_at: Some(records::now_ms() as i64 + 3_600_000), tier: None })),
            AgentKind::Codex => Some(login::Login::Codex(super::tests_codex::auth())),
            _ => None,
        },
    }
}

pub(super) fn boxed(agent: AgentKind) -> Started {
    Started { isolation: Isolation::Container, ..started(3, "claude", agent) }
}

/// An account home with a login of `agent`, and much besides.
pub(super) fn account(agent: AgentKind, home: &Path) {
    let file = |p: &str, text: &str| {
        let at = home.join(p);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, text).unwrap();
    };
    match agent {
        AgentKind::Gemini => {
            file(".gemini/oauth_creds.json", "{}");
            file(".gemini/settings.json", r#"{"model":{"name":"pro"},"hooks":{"x":1},"mcpServers":{"m":{}}}"#);
        }
        AgentKind::Kimi => {
            file("credentials/kimi-code.json", "{}");
            file("oauth/kimi-code", "");
            file("config.toml", "default_model = \"m\"\n[[hooks]]\ncommand = \"evil\"\n[thinking]\neffort = \"max\"\n");
        }
        AgentKind::Codex => super::tests_codex::account(home),
        AgentKind::Claude => {}
    }
}

/// The helper's directory with every file written for `agent`, and its
/// script.
pub(super) fn written(agent: AgentKind, mode: &str) -> (PathBuf, String) {
    let dir = temp(&format!("container-{}", agent.as_str()));
    account(agent, &dir.join("account"));
    write_files(&dir, 41, &args(agent), "proj#12", mode, &ready(agent, &dir.join("account"))).unwrap();
    let run = std::fs::read_to_string(dir.join("run.sh")).unwrap();
    (dir, run)
}

#[test]
fn container_script_mounts_its_three_places_and_removes_itself() {
    let (dir, run) = written(AgentKind::Claude, "auto");
    let d = dir.display();
    let name = docker::container_name(41);
    assert!(name.starts_with("marspot-helper-") && name.ends_with("-41"));
    assert!(run.contains(&format!("trap 'trap \"\" HUP TERM; limit 20 \"$docker\" rm -f {name} >/dev/null 2>&1' EXIT HUP TERM\n")), "{run}");
    assert!(run.contains(" --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 --memory 8g -w "), "{run}");
    assert!(run.contains("\"$docker\" run -it --init --rm --pull never --name "), "{run}");
    assert!(run.contains("--label marspot.helper=41 --label marspot.state="), "{run}");
    assert!(run.contains(&format!("-w /work --mount 'type=bind,source=/Users/someone/proj,target=/work' --mount 'type=bind,source={d}/share,target=/marspot' --mount 'type=bind,source={d}/home,target=/claude' --env-file '{d}/env'")), "{run}");
    assert!(run.contains(&format!("\"$(cat '{d}/image')\" claude --model 'a-model' '--permission-mode' 'bypassPermissions' -- \"$(cat '{d}/task.md')\" </dev/tty &\nwait $!\n")), "{run}");
    assert_eq!((run.matches(" --mount ").count(), run.matches(" -v ").count()), (3, 0), "nothing else of the host");
    let again = std::fs::read_to_string(dir.join("resume.sh")).unwrap();
    assert!(again.contains(" claude --continue ") && !again.contains("task.md"), "{again}");
    for script in ["run.sh", "resume.sh"] {
        let ok = std::process::Command::new("/bin/sh").arg("-n").arg(dir.join(script)).status().unwrap();
        assert!(ok.success(), "{script} parses");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_token_is_in_the_env_file_only() {
    let (dir, _) = written(AgentKind::Claude, "auto");
    let env = dir.join("env");
    assert_eq!(std::fs::read_to_string(&env).unwrap(), format!("CLAUDE_CODE_OAUTH_TOKEN={TOKEN}\n"));
    assert_eq!(std::fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);
    let mut others = 0;
    for e in std::fs::read_dir(&dir).unwrap().chain(std::fs::read_dir(dir.join("home")).unwrap()).flatten() {
        if e.path().is_file() && e.path() != env {
            others += 1;
            assert!(!std::fs::read_to_string(e.path()).unwrap().contains(TOKEN), "{:?}", e.path());
        }
    }
    assert!(others >= 7, "{others} other files were read");
    // written again for a newer login, as private as the first
    let newer = login::ClaudeLogin { token: "another".into(), expires_at: Some(9_000), tier: None };
    login::write_env(&dir, &newer).unwrap();
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "CLAUDE_CODE_OAUTH_TOKEN=another\n");
    assert_eq!(std::fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(login::expires_ms(&dir), Some(9_000));
    let broken = login::ClaudeLogin { token: "two\nlines".into(), expires_at: None, tier: None };
    assert!(login::write_env(&dir, &broken).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_task_names_the_places_inside() {
    let (dir, _) = written(AgentKind::Claude, "auto");
    let task = std::fs::read_to_string(dir.join("task.md")).unwrap();
    assert!(task.starts_with("Task from proj#12, delegated through marspot."));
    assert!(task.contains("Working directory: /work\n") && task.contains("write your final result to /marspot/result.md."));
    assert!(task.contains("Do not use SendMessage"));
    assert!(!task.contains("/Users/"), "{task}");
    assert_eq!(super::super::first_screens::task_dir(&task), Some("/work"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_other_agents_get_their_login_and_nothing_else_of_the_account() {
    let wants: [(AgentKind, &str, &[&str], &str); 2] = [
        (AgentKind::Gemini, "/home/node/.gemini", &[".gemini/oauth_creds.json"], " gemini -m 'a-model' '--approval-mode' 'yolo' '--skip-trust' '--include-directories' '/marspot' -i \"$(cat "),
        (AgentKind::Kimi, "/home/node/.kimi-code", &["credentials", "oauth"], " kimi -m 'a-model' '--auto' </dev/tty &"),
    ];
    for (agent, home_at, logins, words) in wants {
        let (dir, run) = written(agent, "auto");
        let real = std::fs::canonicalize(&dir).unwrap();
        assert!(run.contains(&format!("--mount 'type=bind,source={}/home,target={home_at}'", dir.display())), "{run}");
        for l in logins {
            let inner = l.strip_prefix(".gemini/").unwrap_or(l);
            assert!(run.contains(&format!("--mount 'type=bind,source={}/account/{l},target={home_at}/{inner}'", real.display())), "{run}");
        }
        assert_eq!((run.matches(" -v ").count(), run.matches(" --mount ").count()), (0, 3 + logins.len()), "{run}");
        assert!(run.contains(words), "{run}");
        assert!(!run.contains("--env-file") && !dir.join("env").exists() && !dir.join("resume.sh").exists());
        assert!(!run.contains("marspot-peer"), "no tool of the host: {run}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn container_refuses_what_cannot_go_on_the_line() {
    let dir = temp("container-bad");
    let mut r = ready(AgentKind::Claude, &dir);
    for cwd in ["/a:b", "/it's", "/a,b", "relative"] {
        r.cwd = cwd.into();
        assert!(write_files(&dir, 41, &args(AgentKind::Claude), "p", "auto", &r).is_err(), "{cwd}");
    }
    r.cwd = "/w".into();
    let mut a = args(AgentKind::Claude);
    a.model = Some("m' ; rm".into());
    assert!(write_files(&dir, 41, &a, "p", "auto", &r).is_err());
    assert!(write_files(&dir, 41, &args(AgentKind::Claude), "p", "-c 'x", &r).is_err());
    let mut a = args(AgentKind::Claude);
    a.resume_last = true;
    assert!(check(&a, "/w").is_err_and(|e| e.code == ErrorCode::NotSupported));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_account_needs_a_login_that_outlives_the_budget() {
    let min = |m: i64| m * 60_000;
    assert_eq!(login::left_s(Some(min(50)), min(10) as u64, None), Some(2400));
    assert_eq!(login::left_s(Some(min(50)), min(10) as u64, Some(60)), Some(60), "cut short");
    assert_eq!(login::left_s(None, 0, Some(60)), None);
    assert!(login::lasts(Some(40 * 60), 30));
    assert!(!login::lasts(Some(40 * 60 - 1), 30));
    assert!(!login::lasts(None, 1));
    let text = login::none_text(AgentKind::Claude, &vec![(1, Some(1500)), (2, Some(-4)), (3, None)], 30);
    let dir = temp("container-lasts");
    let until = |m: i64| std::fs::write(dir.join("expires"), (records::now_ms() as i64 + min(m)).to_string()).unwrap();
    until(45);
    assert!(login::still_lasts(&dir, 30));
    until(39);
    assert!(!login::still_lasts(&dir, 30), "by the time it is handed its task");
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(text.contains("lasts 40 min"), "{text}");
    assert!(text.contains("P1 has 25 min left, P2 has expired, P3 has no login"), "{text}");
}

#[test]
fn container_stop_with_an_expired_login_says_so() {
    let refusal = "● Please run /login · API Error: 401 OAuth token has expired";
    let now = 100 * 60_000;
    assert!(expired::judge(AgentKind::Claude, now + 1, None, "● Done.", now).is_none(), "the login is still good");
    assert!(expired::judge(AgentKind::Claude, now + 1, Some(now + 2), refusal, now).is_none(), "claude's early refusal is not looked for");
    let o = expired::judge(AgentKind::Claude, now - 5 * 60_000, None, refusal, now).unwrap();
    assert!(!o.renewed && o.note.contains("expired 5 min ago") && o.note.contains("has not been renewed"), "{}", o.note);
    assert!(!expired::judge(AgentKind::Claude, now - 1, Some(now - 1), refusal, now).unwrap().renewed, "the same login on the host");
    let o = expired::judge(AgentKind::Claude, now - 1, Some(now + 1), refusal, now).unwrap();
    assert!(o.renewed && o.note.contains("started again"), "{}", o.note);
    assert!(!expired::judge(AgentKind::Claude, now - 1, Some(now + 1), "● Done.", now).unwrap().renewed, "not refused: nothing to go on with");

    let dir = temp("container-expired");
    let (s, _rx) = shared_at(&dir);
    s.record(41, Event::Started(boxed(AgentKind::Gemini)));
    let h = s.table().map[&41].clone();
    std::fs::create_dir_all(s.dir_of(41)).unwrap();
    std::fs::write(s.dir_of(41).join("expires"), "5").unwrap();
    assert!(at_stop(&s, &h, Stop::Idle, Some(refusal)).is_none(), "only claude's and codex's are a token");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_helper_is_judged_from_its_screen_and_read_from_its_share() {
    let dir = temp("container-judge");
    let (s, _rx) = shared_at(&dir);
    for (sid, st) in [(41, boxed(AgentKind::Claude)), (42, started(3, "claude", AgentKind::Claude))] {
        s.record(sid, Event::Started(st));
    }
    let t = s.table();
    assert!(!turn::turn_reported(&t.map[&41]) && turn::turn_reported(&t.map[&42]));
    assert_eq!(s.result_path(41), s.dir_of(41).join("share/result.md"));
    assert_eq!(s.result_path(42), s.dir_of(42).join("result.md"));
    assert_eq!(s.own_dir_shown(41), Path::new("/marspot"));
    let text = |h: &Helper| notify::text("n", h, Stop::Idle, Path::new("/r"), None, None, None);
    assert!(text(&t.map[&41]).contains("Ask it more with PaneSend;"));
    assert!(text(&t.map[&42]).contains("SendMessage or PaneSend"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_result_is_never_read_through_a_link_or_a_pipe() {
    let dir = temp("container-result");
    let (secret, result) = (dir.join("secret"), dir.join("result.md"));
    std::fs::write(&secret, "host file").unwrap();
    symlink(&secret, &result).unwrap();
    assert_eq!(notify::read_result(&result), None);
    std::fs::remove_file(&result).unwrap();
    assert!(std::process::Command::new("/usr/bin/mkfifo").arg(&result).status().unwrap().success());
    assert_eq!(notify::read_result(&result), None);
    let h = Helper::new(41, boxed(AgentKind::Claude), 0);
    let told = |r: &Path| notify::text("n", &h, Stop::Idle, r, None, None, None);
    assert!(told(&result).contains("is not a regular file; it was not read"), "{}", told(&result));
    assert!(told(&dir.join("none")).contains("has not written its result file"));
    std::fs::remove_file(&result).unwrap();
    std::fs::write(&result, "x".repeat(notify::MAX_RESULT + 9)).unwrap();
    let text = notify::read_result(&result).unwrap();
    assert!(text.ends_with(&format!("[cut: the file has {} bytes, this is the first 256 KiB]", notify::MAX_RESULT + 9)));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_sweep_takes_only_sessions_that_are_gone() {
    let listing = "marspot-helper-ab-41\t41\nmarspot-helper-ab-42\t42\nother\t\nodd\tx\n";
    assert_eq!(cleanup::orphans(listing, |sid| sid == 42), vec!["marspot-helper-ab-41"]);
    let dir = temp("container-sweep");
    let (s, _rx) = shared_at(&dir);
    s.record(42, Event::Started(started(3, "claude", AgentKind::Claude)));
    assert!(!pending::place(&s.root()).exists(), "no container ever: docker is not asked");
    begin(&s, 41, &args(AgentKind::Claude), "p", "auto", &ready(AgentKind::Claude, &dir)).unwrap();
    assert_eq!(pending::all(&s.root()), [41].into());
    pending::gone(&s.root(), 41);
    assert!(pending::place(&s.root()).is_dir() && pending::all(&s.root()).is_empty() && !s.dir_of(41).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_image_is_named_by_its_content() {
    assert_ne!(image::tag_of(b"FROM a"), image::tag_of(b"FROM b"));
    assert_eq!(image::tag(), image::tag());
    assert!(image::tag().starts_with("marspot-helper:"));
}

/// For bin/test-helper-container.sh: write a helper's files where it
/// says, for the docker and the image it names.
#[test]
#[ignore = "writes files for bin/test-helper-container.sh"]
fn container_files_for_the_script() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let (dir, cwd) = (PathBuf::from(var("CONTAINER_DIR")), var("CONTAINER_CWD"));
    let agent = AgentKind::parse(&var("CONTAINER_AGENT")).unwrap();
    let sid: u64 = var("CONTAINER_SID").parse().unwrap();
    let mut a = args(agent);
    a.model = None;
    let r = Ready { docker: PathBuf::from(var("CONTAINER_DOCKER")), cwd: cwd.into(), ..ready(agent, Path::new(&var("CONTAINER_HOME"))) };
    write_files(&dir, sid, &a, "proj#12", "auto", &r).unwrap();
    std::fs::write(dir.join("image"), var("CONTAINER_IMAGE")).unwrap();
    std::fs::write(dir.join("name"), docker::container_name(sid)).unwrap();
}
