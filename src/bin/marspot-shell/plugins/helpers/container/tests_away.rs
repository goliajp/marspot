//! What a container is kept from, and what is done with a docker that
//! does not do as told: `bin/_fake-docker.sh` in its place.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::tests::temp;
use super::tests::account;
use super::*;

fn bound(agent: AgentKind, home: &Path) -> Result<Vec<creds::Bind>, ErrorCode> {
    creds::of(agent, home).map_err(|r| r.code)
}

#[test]
fn container_planted_links_are_not_mounted() {
    let dir = std::fs::canonicalize(temp("container-planted")).unwrap();
    let (home, secret) = (dir.join("home"), dir.join("secret"));
    std::fs::create_dir_all(secret.join("d")).unwrap();
    account(AgentKind::Kimi, &home);
    account(AgentKind::Gemini, &home);
    // links beside the login: never looked at
    symlink(&secret, home.join("sessions")).unwrap();
    symlink(&secret, home.join(".gemini/tmp")).unwrap();
    let all: Vec<PathBuf> = [AgentKind::Gemini, AgentKind::Kimi].iter().flat_map(|a| bound(*a, &home).unwrap()).map(|b| b.from).collect();
    assert_eq!(all, [home.join(".gemini/oauth_creds.json"), home.join("credentials"), home.join("oauth")]);

    // in the login's place, or among it
    symlink(secret.join("d"), home.join("credentials/more.json")).unwrap();
    assert_eq!(bound(AgentKind::Kimi, &home), Err(ErrorCode::NotSupported));
    std::fs::remove_file(home.join("credentials/more.json")).unwrap();
    std::fs::write(secret.join("f"), "x").unwrap();
    std::fs::hard_link(secret.join("f"), home.join("credentials/hard.json")).unwrap();
    assert_eq!(bound(AgentKind::Kimi, &home), Err(ErrorCode::NotSupported));
    std::fs::remove_file(home.join("credentials/hard.json")).unwrap();
    symlink(secret.join("f"), home.join("oauth/kimi-code2")).unwrap();
    assert_eq!(bound(AgentKind::Kimi, &home), Err(ErrorCode::NotSupported));
    std::fs::remove_file(home.join(".gemini/oauth_creds.json")).unwrap();
    symlink(secret.join("d"), home.join(".gemini/oauth_creds.json")).unwrap();
    assert_eq!(bound(AgentKind::Gemini, &home), Err(ErrorCode::NoAccount), "a directory");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_account_with_no_login_is_refused_and_nothing_is_made() {
    let home = temp("container-no-login");
    for agent in [AgentKind::Gemini, AgentKind::Kimi] {
        assert_eq!(bound(agent, &home), Err(ErrorCode::NoAccount));
    }
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn container_home_has_the_model_and_no_hook() {
    let host = "default_model = \"k/a\"\nnotify = [\"x\"]\n[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\napi_key = \"sk-1\"\nbase_url = \"https://h\"\n\
                [providers.\"managed:kimi-code\".oauth]\nkey = \"oauth/kimi-code\"\n[providers.other]\nbase_url = \"https://o\"\napi_key = \"sk-2\"\n\
                [models.\"k/a\"]\nprovider = \"managed:kimi-code\"\ncapabilities = [\n  \"a\",\n]\nmax_context_size = 1\n[thinking]\neffort = \"max\"\n\
                [[hooks]]\nevent = \"Stop\"\ncommand = \"evil\"\n[mcp]\ntype = \"x\"\n";
    let want = "default_model = \"k/a\"\n\n[providers.\"managed:kimi-code\"]\napi_key = \"\"\ntype = \"kimi\"\nbase_url = \"https://h\"\n\n\
                [providers.\"managed:kimi-code\".oauth]\nkey = \"oauth/kimi-code\"\n\n[models.\"k/a\"]\nprovider = \"managed:kimi-code\"\nmax_context_size = 1\n\n\
                [thinking]\neffort = \"max\"\n";
    assert_eq!(home::kimi_config(host), want);
    let host = r#"{"model":{"name":"pro"},"hooks":{"a":1},"mcpServers":{"m":{}},"security":{"auth":{"selectedType":"gemini-api-key"}}}"#;
    assert_eq!(home::gemini_settings(host), r#"{"security":{"auth":{"selectedType":"oauth-personal"}},"model":{"name":"pro"}}"#);
    assert_eq!(home::gemini_settings("not json"), r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#);
}

#[test]
fn container_may_not_have_the_hosts_own_directories() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let state = marspot_term::paths::state_root();
    std::fs::create_dir_all(state.join("helpers")).unwrap();
    let dir = temp("container-cwd");
    symlink(&state, dir.join("link")).unwrap();
    let data = Path::new("/System/Volumes/Data").join(home.strip_prefix("/").unwrap());
    let no = [Path::new("/").into(), home.clone(), home.parent().unwrap().into(), "/System/Volumes".into(), data, state.clone(), state.join("helpers"), dir.join("link")];
    for p in no.iter().filter(|p| p.is_dir()) {
        assert!(guard::cwd(p).is_err_and(|r| r.code == ErrorCode::BadRequest), "{}", p.display());
    }
    let mut env = marspot_term::test_env::EnvGuard::new();
    let home = dir.join("home");
    for p in [".ssh/inner", "Library/Caches", "proj/.hidden", "elsewhere"] {
        std::fs::create_dir_all(home.join(p)).unwrap();
    }
    symlink(home.join("elsewhere"), home.join(".linked")).unwrap();
    env.set("HOME", &home);
    for p in [".ssh", ".ssh/inner", "Library", "Library/Caches", ".linked", "elsewhere"] {
        assert!(guard::cwd(&home.join(p)).is_err_and(|r| r.code == ErrorCode::BadRequest), "{p}");
    }
    for p in ["proj", "proj/.hidden"] {
        assert!(guard::cwd(&home.join(p)).is_ok(), "{p}");
    }
    drop(env);
    assert!(guard::cwd(Path::new("relative")).is_err());
    assert_eq!(guard::cwd(&dir).unwrap(), std::fs::canonicalize(&dir).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_git_config_and_hooks_are_read_only() {
    let dir = temp("container-git");
    assert!(guard::git_parts(&dir).is_empty());
    std::fs::create_dir_all(dir.join("real/hooks")).unwrap();
    symlink(dir.join("real"), dir.join(".git")).unwrap();
    assert!(guard::git_parts(&dir).is_empty(), "a link is left alone");
    std::fs::remove_file(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git"), "gitdir: /elsewhere").unwrap();
    assert_eq!(guard::git_parts(&dir), [".git"]);
    std::fs::remove_file(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    symlink("/etc/passwd", dir.join(".git/config")).unwrap();
    assert_eq!(guard::git_parts(&dir), [".git/hooks"]);
    std::fs::remove_file(dir.join(".git/config")).unwrap();
    std::fs::write(dir.join(".git/config"), "").unwrap();
    let spec = script::Spec { docker: Path::new("/usr/bin/true"), dir: &dir, sid: 4, agent: AgentKind::Kimi, binds: &[], model: None, mode: "auto", cwd: &dir };
    let run = script::run(&spec, false).unwrap();
    let d = dir.display();
    for part in [".git/config", ".git/hooks"] {
        assert!(run.contains(&format!("--mount 'type=bind,source={d}/{part},target=/work/{part},readonly'")), "{run}");
        assert!(run.contains(&format!("[ -L '{d}/{part}' ] && exit 1\n")), "{run}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A docker that is the stand-in, with its files in `dir`.
pub(super) fn fake(dir: &Path) -> PathBuf {
    let (bin, real) = (dir.join("docker"), Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/_fake-docker.sh"));
    std::fs::write(&bin, format!("#!/bin/sh\nexport FAKE_DOCKER_DIR='{}' FAKE_DOCKER_BUILD_S=0\nexec bash '{}' \"$@\"\n", dir.display(), real.display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(docker::run(&bin, &["version"], None, docker::ANSWER).unwrap().ok);
    bin
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

#[test]
fn container_removal_that_docker_refuses_is_an_error() {
    let dir = temp("container-rm");
    let d = fake(&dir);
    assert_eq!(docker::remove(&d, "marspot-helper-x-1"), Ok(()));
    std::fs::write(dir.join("rm-says"), "No such container: x").unwrap();
    assert_eq!(docker::remove(&d, "marspot-helper-x-1"), Ok(()), "not there is as good as removed");
    std::fs::write(dir.join("rm-says"), "removal in progress").unwrap();
    assert!(docker::remove(&d, "marspot-helper-x-1").is_err_and(|e| e.contains("removal in progress")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_sweep_removes_what_is_left_of_gone_helpers() {
    let dir = temp("container-swept");
    let d = fake(&dir);
    let mut left = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
    std::fs::write(dir.join("ps/marspot-helper-x-41"), format!("{}\n41\n", left.id())).unwrap();
    std::fs::write(dir.join("ps/marspot-helper-x-42"), "0\n42\n").unwrap();
    cleanup::sweep_now(&dir, &d, |sid| sid == 42);
    assert!(!dir.join("ps/marspot-helper-x-41").exists() && dir.join("ps/marspot-helper-x-42").exists());
    assert!(!left.wait().unwrap().success(), "ended");
    assert!(log(&dir).contains("ps -a --filter label=marspot.state="));
    // a docker whose answer is an error is not believed
    std::fs::write(dir.join("ps-fails"), "").unwrap();
    cleanup::sweep_now(&dir, &d, |_| false);
    assert!(dir.join("ps/marspot-helper-x-42").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_image_build_removes_the_older_ones() {
    let dir = temp("container-images");
    let d = fake(&dir);
    for old in ["marspot-helper_old", "marspot-helper_used", "other_old"] {
        std::fs::write(dir.join("images").join(old), "").unwrap();
    }
    std::fs::write(dir.join("images-used"), "marspot-helper_used").unwrap();
    let soon = || std::time::Instant::now() + docker::ANSWER;
    assert_eq!(image::present(&d, soon()), Ok(false));
    image::ready(&d).unwrap();
    assert_eq!(image::present(&d, soon()), Ok(true));
    let there = |n: &str| dir.join("images").join(n).exists();
    assert!(!there("marspot-helper_old") && there("marspot-helper_used") && there("other_old"));
    let log = log(&dir);
    assert!(log.contains("build --label marspot.helper.image=1 --label marspot.state="), "{log}");
    assert!(log.contains("image ls --filter label=marspot.helper.image=1 --filter label=marspot.state="), "{log}");
    assert!(!log.contains(" -f marspot-helper:"), "never by force: {log}");
    image::ready(&d).unwrap();
    assert_eq!(log.matches("build --label").count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn container_checks_share_one_time_limit() {
    let dir = temp("container-slow");
    let d = dir.join("docker");
    std::fs::write(&d, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    let t = std::time::Instant::now();
    let until = t + std::time::Duration::from_millis(400);
    for _ in 0..3 {
        assert!(image::present(&d, until).is_err_and(|r| r.code == ErrorCode::NoContainerRuntime));
    }
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    std::fs::remove_dir_all(&dir).unwrap();
}
