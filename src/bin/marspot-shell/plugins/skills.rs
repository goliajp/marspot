//! The agent skills marspot ships, kept in every agent's skills
//! directory.
//!
//! `marspot-profiles`: how to lay a machine out as one shared directory
//! plus one directory per account, which is the layout the profile
//! switch in the claudecode and codex plugins needs.  `marspot-panes`:
//! the `marspot-peer pane` command line, for a session the user started
//! by hand and so has no pane tools over MCP.  `marspot-helpers`: when to
//! hand work to a helper rather than a one-shot `exec`, over either the
//! MCP tools or `marspot-peer helper`.  Their files are compiled
//! in and written wherever they are missing or differ, so an update of
//! marspot updates the skills.
//!
//! Only an agent directory that already exists is written under: a
//! machine without codex does not get a `~/.codex`.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Skill name, and its files: file name, text, mode.
type Skill = (&'static str, &'static [(&'static str, &'static str, u32)]);

const SKILLS: &[Skill] = &[
    (
        "marspot-profiles",
        &[
            ("SKILL.md", include_str!("../../../../assets/skills/marspot-profiles/SKILL.md"), 0o644),
            ("profiles.sh", include_str!("../../../../assets/skills/marspot-profiles/profiles.sh"), 0o755),
        ],
    ),
    ("marspot-panes", &[("SKILL.md", include_str!("../../../../assets/skills/marspot-panes/SKILL.md"), 0o644)]),
    ("marspot-helpers", &[("SKILL.md", include_str!("../../../../assets/skills/marspot-helpers/SKILL.md"), 0o644)]),
];

/// Every file of every skill, under its skill's directory name.
fn all_files() -> impl Iterator<Item = (&'static str, &'static str, &'static str, u32)> {
    SKILLS.iter().flat_map(|(skill, files)| files.iter().map(move |(name, text, mode)| (*skill, *name, *text, *mode)))
}

/// Bring every agent's copy of the skills in line with the compiled-in
/// one.  Returns lines worth logging: what was written, what failed.
pub(crate) fn reconcile() -> Vec<String> {
    // a sandbox shares the developer's HOME, and these are the
    // developer's own agent directories
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return Vec::new();
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let gemini_home = std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from);
    install_into(&skills_dirs(&home, gemini_home.as_deref()))
}

/// The skills directory of every agent config directory that exists,
/// each once.
///
/// Profile directories commonly symlink one shared `skills`;
/// canonicalising means it is written once rather than once per
/// profile.
fn skills_dirs(home: &Path, gemini_home: Option<&Path>) -> Vec<PathBuf> {
    // `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `KIMI_CODE_HOME` name the
    // config directory itself; `GEMINI_CLI_HOME` names a home with
    // `.gemini` inside
    let mut configs = vec![home.join(".claude"), home.join(".codex"), home.join(".gemini"), home.join(".kimi-code")];
    if let Some(g) = gemini_home {
        configs.push(g.join(".gemini"));
    }
    if let Ok(rd) = fs::read_dir(home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".claude-profile-") || name.starts_with(".codex-profile-") || name.starts_with(".kimi-code-profile-") {
                configs.push(e.path());
            } else if name.starts_with(".gemini-profile-") {
                configs.push(e.path().join(".gemini"));
            }
        }
    }
    configs.sort();
    let mut out: Vec<PathBuf> = Vec::new();
    for config in configs {
        let Ok(config) = config.canonicalize() else { continue };
        if !config.is_dir() {
            continue;
        }
        let skills = config.join("skills");
        // a `skills` that does not exist yet is created in the config
        // directory; one that does is followed to where it really is
        let skills = skills.canonicalize().unwrap_or(skills);
        if !out.contains(&skills) {
            out.push(skills);
        }
    }
    out
}

/// The skills' files wherever `reconcile` keeps them.
pub(crate) fn files() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let gemini_home = std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from);
    let dirs = skills_dirs(&home, gemini_home.as_deref());
    dirs.iter().flat_map(|d| all_files().map(move |(skill, name, _, _)| d.join(skill).join(name))).collect()
}

fn install_into(skills_dirs: &[PathBuf]) -> Vec<String> {
    let mut notes = Vec::new();
    for skills in skills_dirs {
        for (skill, name, text, mode) in all_files() {
            let path = skills.join(skill).join(name);
            match install_file(&path, text, mode) {
                Ok(true) => notes.push(format!("{}: written", path.display())),
                Ok(false) => {}
                Err(e) => notes.push(format!("{}: {e}", path.display())),
            }
        }
    }
    notes
}

/// True when the file had to be written.
fn install_file(path: &Path, text: &str, mode: u32) -> io::Result<bool> {
    if let (Ok(have), Ok(md)) = (fs::read(path), fs::metadata(path))
        && have == text.as_bytes()
        && md.permissions().mode() & 0o777 == mode
    {
        return Ok(false);
    }
    let dir = path.parent().expect("a file inside the skill directory");
    fs::create_dir_all(dir)?;
    // renamed into place: an agent reading the skill never sees half
    // of it
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().expect("a file name").to_string_lossy(), std::process::id()));
    fs::write(&tmp, text)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILES: &str = "marspot-profiles";

    fn text_of(skill: &str, name: &str) -> &'static str {
        all_files().find(|f| f.0 == skill && f.1 == name).expect("a compiled-in file").2
    }

    fn home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("skills-{}", marspot_peer::rand::hex(6)));
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn installed(skills: &Path) -> (PathBuf, PathBuf) {
        (skills.join(PROFILES).join("SKILL.md"), skills.join(PROFILES).join("profiles.sh"))
    }

    #[test]
    fn skills_missing_are_written_and_the_script_is_executable() {
        let h = home();
        fs::create_dir_all(h.join(".claude")).unwrap();
        let notes = install_into(&skills_dirs(&h, None));
        assert_eq!(notes.len(), 4, "{notes:?}");
        let (md, sh) = installed(&h.join(".claude/skills"));
        assert_eq!(fs::read_to_string(&md).unwrap(), text_of(PROFILES, "SKILL.md"));
        assert_eq!(fs::read_to_string(&sh).unwrap(), text_of(PROFILES, "profiles.sh"));
        assert_eq!(fs::metadata(&sh).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(&md).unwrap().permissions().mode() & 0o777, 0o644);
        // nothing of the write is left beside them
        assert_eq!(fs::read_dir(md.parent().unwrap()).unwrap().count(), 2);
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_identical_are_left_untouched() {
        let h = home();
        fs::create_dir_all(h.join(".codex")).unwrap();
        install_into(&skills_dirs(&h, None));
        let (md, sh) = installed(&h.join(".codex/skills"));
        let before = (fs::metadata(&md).unwrap().modified().unwrap(), fs::metadata(&sh).unwrap().modified().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(install_into(&skills_dirs(&h, None)).is_empty());
        let after = (fs::metadata(&md).unwrap().modified().unwrap(), fs::metadata(&sh).unwrap().modified().unwrap());
        assert_eq!(before, after);
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_that_differ_are_replaced() {
        let h = home();
        fs::create_dir_all(h.join(".claude")).unwrap();
        install_into(&skills_dirs(&h, None));
        let (md, sh) = installed(&h.join(".claude/skills"));
        fs::write(&md, "an older text").unwrap();
        fs::set_permissions(&sh, fs::Permissions::from_mode(0o644)).unwrap();
        let notes = install_into(&skills_dirs(&h, None));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(fs::read_to_string(&md).unwrap(), text_of(PROFILES, "SKILL.md"));
        assert_eq!(fs::metadata(&sh).unwrap().permissions().mode() & 0o777, 0o755);
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_are_not_installed_for_an_agent_that_is_absent() {
        let h = home();
        fs::create_dir_all(h.join(".claude")).unwrap();
        fs::create_dir_all(h.join("elsewhere")).unwrap();
        assert_eq!(skills_dirs(&h, None), vec![h.join(".claude/skills")]);
        // a GEMINI_CLI_HOME with no .gemini in it is not an agent directory either
        assert_eq!(skills_dirs(&h, Some(&h.join("elsewhere"))), vec![h.join(".claude/skills")]);
        install_into(&skills_dirs(&h, None));
        let mut names: Vec<_> =
            fs::read_dir(&h).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, [".claude", "elsewhere"]);
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_shared_through_symlinks_are_written_once() {
        let h = home();
        for d in [".claude-shared/skills", ".claude-profile-1", ".claude-profile-2", ".codex-profile-1"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        for p in [".claude-profile-1", ".claude-profile-2"] {
            std::os::unix::fs::symlink(h.join(".claude-shared/skills"), h.join(p).join("skills")).unwrap();
        }
        std::os::unix::fs::symlink(h.join(".claude-profile-1"), h.join(".claude")).unwrap();
        std::os::unix::fs::symlink(h.join(".codex-profile-1"), h.join(".codex")).unwrap();
        let dirs = skills_dirs(&h, None);
        assert_eq!(dirs, vec![h.join(".claude-shared/skills"), h.join(".codex-profile-1/skills")]);
        assert_eq!(install_into(&dirs).len(), 8);
        // reached through either profile, and still a symlink
        assert!(h.join(".claude-profile-2/skills").join(PROFILES).join("profiles.sh").is_file());
        assert!(h.join(".claude-profile-1/skills").symlink_metadata().unwrap().file_type().is_symlink());
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_for_kimi_sit_in_each_kimi_home() {
        let h = home();
        for d in [".kimi-code", ".kimi-code-profile-2"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        let dirs = skills_dirs(&h, None);
        assert_eq!(dirs, vec![h.join(".kimi-code/skills"), h.join(".kimi-code-profile-2/skills")]);
        install_into(&dirs);
        assert!(installed(&h.join(".kimi-code-profile-2/skills")).0.is_file());
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn skills_for_gemini_sit_one_level_down() {
        let h = home();
        for d in [".gemini", ".gemini-profile-2/.gemini", ".gemini-profile-3", "g/.gemini"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        let dirs = skills_dirs(&h, Some(&h.join("g")));
        assert_eq!(
            dirs,
            vec![h.join(".gemini/skills"), h.join(".gemini-profile-2/.gemini/skills"), h.join("g/.gemini/skills")]
        );
        install_into(&dirs);
        assert!(installed(&h.join(".gemini-profile-2/.gemini/skills")).0.is_file());
        assert!(!h.join(".gemini-profile-3/.gemini").exists());
        fs::remove_dir_all(&h).unwrap();
    }
    /// The pane-control skill goes to all four agents, each into its
    /// own skills directory, beside the profiles skill.
    #[test]
    fn skills_include_marspot_panes_for_all_four_agents() {
        let h = home();
        for d in [".claude", ".codex", ".gemini", ".kimi-code"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        let dirs = skills_dirs(&h, None);
        assert_eq!(dirs.len(), 4, "{dirs:?}");
        install_into(&dirs);
        for skills in [".claude/skills", ".codex/skills", ".gemini/skills", ".kimi-code/skills"] {
            let md = h.join(skills).join("marspot-panes/SKILL.md");
            let text = fs::read_to_string(&md).unwrap();
            assert_eq!(text, text_of("marspot-panes", "SKILL.md"));
            assert!(text.starts_with("---\nname: marspot-panes\n"), "{text}");
            assert!(installed(&h.join(skills)).0.is_file(), "the profiles skill is still there");
        }
        fs::remove_dir_all(&h).unwrap();
    }

    /// It is read only when used, but it still enters the context then:
    /// short, and it documents only commands the command line accepts.
    #[test]
    fn skills_marspot_panes_is_short_and_names_real_commands() {
        let text = text_of("marspot-panes", "SKILL.md");
        assert!(text.len() <= 4096, "{} bytes", text.len());
        for sub in ["list", "open", "send", "keys", "run", "read", "resize", "place", "close", "audit"] {
            assert!(text.contains(&format!("marspot-peer pane {sub}")), "{sub}");
            assert!(marspot_peer::control::cli::USAGE.contains(&format!("marspot-peer pane {sub}")), "{sub}");
        }
        // handing work to another agent is the helpers skill's
        assert!(text.contains("`marspot-helpers` skill"));
    }

    /// The helpers skill goes to all four agents beside the other two.
    #[test]
    fn skills_include_marspot_helpers_for_all_four_agents() {
        let h = home();
        for d in [".claude", ".codex", ".gemini", ".kimi-code"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        install_into(&skills_dirs(&h, None));
        for skills in [".claude/skills", ".codex/skills", ".gemini/skills", ".kimi-code/skills"] {
            let text = fs::read_to_string(h.join(skills).join("marspot-helpers/SKILL.md")).unwrap();
            assert_eq!(text, text_of("marspot-helpers", "SKILL.md"));
            assert!(text.starts_with("---\nname: marspot-helpers\n"), "{text}");
            assert!(h.join(skills).join("marspot-panes/SKILL.md").is_file());
        }
        fs::remove_dir_all(&h).unwrap();
    }

    /// Its description is what makes an agent load it, and its body
    /// documents only what the command line and the MCP tools accept.
    #[test]
    fn skills_marspot_helpers_triggers_and_names_real_arguments() {
        let text = text_of("marspot-helpers", "SKILL.md");
        assert!(text.len() <= 4096, "{} bytes", text.len());
        let description = text.lines().find(|l| l.starts_with("description: ")).expect("a description");
        for phrase in ["Use before running `codex exec`", "gemini -p", "use codex / gemini / kimi / another claude", "have codex draw", "in parallel", "ask another agent", "用 codex"] {
            assert!(description.contains(phrase), "{phrase}");
        }
        let usage = marspot_peer::control::cli_helper::USAGE;
        for sub in ["start --agent", "list [--all]", "stop"] {
            assert!(text.contains(&format!("marspot-peer helper {sub}")), "{sub}");
            assert!(usage.contains(&format!("marspot-peer helper {sub}")), "{sub}");
        }
        for flag in ["--model", "--cwd", "--profile", "--mode", "--resume", "--budget", "--name", "--container"] {
            assert!(text.contains(flag) && usage.contains(flag), "{flag}");
        }
        for tool in ["HelperStart", "Helpers", "HelperStop"] {
            assert!(text.contains(tool), "{tool}");
            assert!(marspot_peer::mcp_panes::TOOLS.iter().any(|(n, _)| *n == tool), "{tool}");
        }
        for arg in ["agent", "task", "cwd", "model", "profile", "mode", "resume", "budget_minutes", "name", "isolation"] {
            assert!(text.contains(&format!("`{arg}")), "{arg}");
        }
        // the answer fields it tells the caller to read
        for field in ["result_path", "others_in_cwd"] {
            assert!(text.contains(field), "{field}");
        }
    }
}
