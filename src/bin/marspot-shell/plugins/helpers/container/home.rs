//! The agent's home in a container: this helper's own, made here, with
//! the choice of a model and what keeps its first-run questions away.
//! None of the account's hooks, tool servers, plugins, rules or skills.

use std::path::Path;

use marspot_peer::control::AgentKind;
use marspot_peer::json::{self, Value, obj};

/// The tables and keys of kimi's config that are kept: which models
/// there are and how the managed service is reached.  No API key, no
/// hook, no other service.
const KIMI_TABLES: [&str; 4] = ["[models.", "[thinking]", "[providers.\"managed:kimi-code\"", "[services.moonshot_"];
const KIMI_KEYS: &str = " default_model default_thinking type base_url storage key oauth_host provider model max_context_size max_output_size \
                         capabilities display_name support_efforts default_effort enabled effort ";

/// Whether `v` is a whole value: one that goes on in the next line is
/// left out, and its other lines have no key.
fn whole(v: &str) -> bool {
    !v.is_empty() && !v.contains("\"\"\"") && !v.contains("'''") && (!v.starts_with('[') || v.ends_with(']')) && (!v.starts_with('{') || v.ends_with('}'))
}

pub(super) fn kimi_config(host: &str) -> String {
    let (mut out, mut inside) = (String::new(), true);
    for l in host.lines().map(str::trim) {
        if l.starts_with('[') && l.ends_with(']') {
            inside = KIMI_TABLES.iter().any(|t| l.starts_with(t));
            if inside {
                out.push_str(&format!("\n{l}\n"));
                let service = l.starts_with("[services.") || l.starts_with("[providers.");
                if service && !l.ends_with(".oauth]") {
                    // as kimi writes it: the login is the oauth one
                    out.push_str("api_key = \"\"\n");
                }
            }
            continue;
        }
        let kept = l.split_once('=').is_some_and(|(k, v)| KIMI_KEYS.contains(&format!(" {} ", k.trim())) && whole(v.trim()));
        if inside && kept {
            out.push_str(&format!("{l}\n"));
        }
    }
    out
}

/// codex's config: the account's choice of model and effort, from the
/// top of its own, and no notice of a newer version to answer.
pub(super) fn codex_config(host: &str) -> String {
    let mut out = String::from("check_for_update_on_startup = false\n");
    for l in host.lines().map(str::trim).take_while(|l| !l.starts_with('[')) {
        let kept = l.split_once('=').is_some_and(|(k, v)| matches!(k.trim(), "model" | "model_reasoning_effort") && whole(v.trim()));
        if kept {
            out.push_str(&format!("{l}\n"));
        }
    }
    out
}

pub(super) fn gemini_settings(host: &str) -> String {
    let model = json::parse(host).ok().and_then(|v| v.at(&["model", "name"]).and_then(Value::str).map(str::to_string));
    let auth = obj([("auth", obj([("selectedType", "oauth-personal".into())]))]);
    let mut v = obj([("security", auth)]);
    if let Some(m) = model {
        v.set("model", obj([("name", m.into())]));
    }
    v.to_json()
}

/// Write the home of `agent`, whose account is at `account`, into `dir`.
pub(super) fn write(agent: AgentKind, dir: &Path, account: &Path) -> std::io::Result<()> {
    let of_host = |p: &str| std::fs::read_to_string(account.join(p)).unwrap_or_default();
    let file = |name: &str, text: &str| std::fs::write(dir.join(name), text);
    std::fs::create_dir_all(dir)?;
    match agent {
        AgentKind::Claude => {
            file(".claude.json", r#"{"hasCompletedOnboarding":true,"projects":{"/work":{"hasTrustDialogAccepted":true}}}"#)?;
            file("settings.json", r#"{"theme":"dark","skipDangerousModePermissionPrompt":true}"#)
        }
        AgentKind::Gemini => {
            file("settings.json", &gemini_settings(&of_host(".gemini/settings.json")))?;
            file("trustedFolders.json", r#"{"/work":"TRUST_FOLDER"}"#)
        }
        AgentKind::Kimi => file("config.toml", &kimi_config(&of_host("config.toml"))),
        AgentKind::Codex => file("config.toml", &codex_config(&of_host("config.toml"))),
    }
}
