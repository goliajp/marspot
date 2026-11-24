//! The script a container helper's pane runs.

use std::path::Path;

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::{SHARE, WORK, creds::Bind, docker, guard};

/// How long the script waits for docker to remove the container.
const REMOVE_S: u32 = 20;
/// seconds, command…: at the limit the command's whole group is killed.
/// An alarm set before an exec does nothing: docker takes no notice of it.
const LIMIT: &str = r#"my $t = shift; my $p = fork; die unless defined $p;
    if (!$p) { setpgrp; exec @ARGV; exit 127 }
    $SIG{ALRM} = sub { kill "KILL", -$p; exit 124 };
    alarm $t; waitpid $p, 0; exit($? & 127 ? 128 + ($? & 127) : $? >> 8)"#;
/// What the container may do and take: no privilege is needed as its
/// user is an ordinary one, and a runaway must not take the machine.
const LIMITS: &str = "--cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 --memory 8g";
/// What a restarted container is told.
const GO_ON: &str = "Your login was renewed and this session was restarted. Go on with the task where you stopped.";

pub(super) struct Spec<'a> {
    pub docker: &'a Path,
    /// The helper's directory on the host.
    pub dir: &'a Path,
    pub sid: u64,
    pub agent: AgentKind,
    /// The account's credentials.
    pub binds: &'a [Bind],
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub cwd: &'a Path,
}

fn bad(m: impl Into<String>) -> Reject {
    Reject::new(ErrorCode::BadRequest, m)
}

/// `s` as one word of the script.
fn word(s: &str) -> Result<String, Reject> {
    if s.is_empty() || s.contains(['\'', '\n', '\r', '\0']) {
        return Err(bad(format!("{s:?} cannot go on a command line")));
    }
    Ok(format!("'{s}'"))
}

/// `p` as docker's `--mount` and `--env-file` and the script take it.
pub(super) fn mountable(p: &Path) -> Result<&str, Reject> {
    let fits = |s: &&str| p.is_absolute() && !s.contains([':', '\'', ',', '"', '\n', '\r']);
    p.to_str().filter(fits).ok_or_else(|| bad(format!("{} cannot be mounted into a container", p.display())))
}

/// Where the agent's home is in the container.
pub(super) fn home_at(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "/claude",
        AgentKind::Codex => "/home/node/.codex",
        AgentKind::Gemini => "/home/node/.gemini",
        AgentKind::Kimi => "/home/node/.kimi-code",
    }
}

fn env(agent: AgentKind) -> &'static [&'static str] {
    match agent {
        AgentKind::Claude => &["CLAUDE_CONFIG_DIR=/claude"],
        AgentKind::Codex => &[],
        // this gemini re-execs itself and the new process misreads its arguments
        AgentKind::Gemini => &["GEMINI_CLI_NO_RELAUNCH=true"],
        AgentKind::Kimi => &["KIMI_CODE_HOME=/home/node/.kimi-code", "KIMI_CODE_TRUST_WORKSPACE=1"],
    }
}

fn mode_flags(agent: AgentKind, mode: &str) -> Result<Vec<String>, Reject> {
    let own: Vec<&str> = match (agent, mode) {
        (AgentKind::Claude, "auto") => vec!["--permission-mode", "bypassPermissions"],
        (AgentKind::Gemini, "auto") => vec!["--approval-mode", "yolo"],
        (AgentKind::Kimi, "auto") => vec!["--auto"],
        (AgentKind::Codex, "auto") => vec!["-a", "never", "-s", FULL],
        (AgentKind::Codex, "ask") => vec!["-s", FULL],
        (AgentKind::Codex, custom) => codex_custom(custom)?,
        (_, "ask") => Vec::new(),
        (_, custom) => custom.split_whitespace().collect(),
    };
    let more: &[&str] = match agent {
        AgentKind::Gemini => &["--skip-trust", "--include-directories", SHARE],
        AgentKind::Claude | AgentKind::Codex | AgentKind::Kimi => &[],
    };
    Ok(own.iter().chain(more).map(|f| (*f).to_string()).collect())
}

/// codex's own sandbox cannot start under docker's seccomp, so every
/// command in it would fail: in a container the container is the
/// sandbox, and codex is told to use none.
const FULL: &str = "danger-full-access";

/// Arguments given for codex in a container, with no sandbox of its own.
fn codex_custom(custom: &str) -> Result<Vec<&str>, Reject> {
    let words: Vec<&str> = custom.split_whitespace().collect();
    let other = |w: &[&str]| match w {
        ["-s" | "--sandbox", v, ..] => *v != FULL,
        [w, ..] => w.strip_prefix("--sandbox=").is_some_and(|v| v != FULL) || *w == "--full-auto" || (w.contains("sandbox_mode=") && !w.trim_end_matches(['"', '\'']).ends_with(FULL)),
        [] => false,
    };
    if (0..words.len()).any(|i| other(&words[i..])) {
        let why = "codex's own sandbox cannot start in a container, so every command in it would fail: in a container leave its sandbox out \
                   (the container is the sandbox), or use isolation host";
        return Err(bad(why));
    }
    let given = |w: &&str| matches!(*w, "-s" | "--sandbox" | "--dangerously-bypass-approvals-and-sandbox" | "--yolo") || w.starts_with("--sandbox=") || w.contains("sandbox_mode=");
    let mut out = words.clone();
    if !words.iter().any(given) {
        out.extend(["-s", FULL]);
    }
    Ok(out)
}

/// The agent and its arguments in the container.
fn agent_words(s: &Spec, again: bool) -> Result<Vec<String>, Reject> {
    let mut w = vec![s.agent.as_str().to_string()];
    if again {
        // the home is this helper's own: its last conversation is the one
        let go_on: &[&str] = if s.agent == AgentKind::Codex { &["resume", "--last"] } else { &["--continue"] };
        w.extend(go_on.iter().map(|g| (*g).to_string()));
    }
    if let Some(m) = s.model {
        let flag = if s.agent == AgentKind::Claude { "--model" } else { "-m" };
        w.extend([flag.into(), word(m)?]);
    }
    for f in mode_flags(s.agent, s.mode)? {
        w.push(word(&f)?);
    }
    let task = if again { word(GO_ON)? } else { format!("\"$(cat {})\"", word(mountable(&s.dir.join("task.md"))?)?) };
    match s.agent {
        AgentKind::Claude => w.extend(["--".into(), task]),
        AgentKind::Codex => w.push(task),
        AgentKind::Gemini => w.extend(["-i".into(), task]),
        // pasted once its screen is up
        AgentKind::Kimi => {}
    }
    Ok(w)
}

/// The script; `again`: the one that starts the container again, with a
/// new token, to go on with the conversation.
///
/// docker runs behind `wait`, which a hangup interrupts: a shell that
/// waits for a command in front runs its trap only once that ends, and
/// docker does not see the terminal go while the agent prints nothing.
/// Another hangup must not end the removal.
pub(super) fn run(s: &Spec, again: bool) -> Result<String, Reject> {
    // unlike -v, an error when the host's is gone, not a new directory there
    let mount = |host: &Path, to: &str| Ok::<_, Reject>(format!("--mount {}", word(&format!("type=bind,source={},target={to}", mountable(host)?))?));
    let cwd = word(mountable(s.cwd)?)?;
    let name = docker::container_name(s.sid);
    let mut opts = vec![
        "-it --init --rm --pull never".to_string(),
        format!("--name {name} --label marspot.helper={} --label {}", s.sid, docker::state_label()),
        LIMITS.to_string(),
        format!("-w {WORK}"),
        mount(s.cwd, WORK)?,
        mount(&s.dir.join("share"), SHARE)?,
        mount(&s.dir.join("home"), home_at(s.agent))?,
    ];
    // a directory written to from a container: what is in the place of
    // these when the script runs may be a link to anything
    let mut links = format!("[ \"$(cd {cwd} && pwd -P)\" = {cwd} ] || {{ echo 'marspot: the working directory is not where it was'; exit 1; }}\n");
    let parts = guard::git_parts(s.cwd);
    for part in &parts {
        let at = s.cwd.join(part);
        links.push_str(&format!("[ -L {} ] && exit 1\n", word(mountable(&at)?)?));
        opts.push(mount(&at, &format!("{WORK}/{part},readonly"))?);
    }
    if !parts.is_empty() {
        links.push_str(&format!("[ -L {} ] && exit 1\n", word(mountable(&s.cwd.join(".git"))?)?));
    }
    if s.agent == AgentKind::Claude {
        opts.push(format!("--env-file {}", word(mountable(&s.dir.join("env"))?)?));
    }
    for b in s.binds {
        opts.push(mount(&b.from, b.to)?);
    }
    opts.push("-e TERM=xterm-256color".into());
    opts.extend(env(s.agent).iter().map(|e| format!("-e {e}")));
    let image = format!("\"$(cat {})\"", word(mountable(&s.dir.join("image"))?)?);
    Ok(format!(
        "#!/bin/sh\ndocker={}\n{links}limit() {{\n  /usr/bin/perl -e '{LIMIT}' \"$@\"\n}}\ntrap 'trap \"\" HUP TERM; limit {REMOVE_S} \"$docker\" rm -f {name} >/dev/null 2>&1' EXIT HUP TERM\n\"$docker\" run {} {image} {} </dev/tty &\nwait $!\n",
        word(mountable(s.docker)?)?,
        opts.join(" "),
        agent_words(s, again)?.join(" "),
    ))
}
