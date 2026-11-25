//! Processes no rule ever names, whatever they look like.

use std::path::Path;

/// System programs and app bundles' own processes.  Tools under an
/// app's `Contents/Developer` (xcodebuild, simctl, sourcekit-lsp) are
/// not excluded: an agent runs those as tasks.
pub fn exe(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.starts_with("/System/")
        || s.starts_with("/usr/libexec/")
        || s.contains(".app/Contents/MacOS/")
        || s.contains(".app/Contents/Frameworks/")
}

/// A process holding or waiting for the machine's measurement lock, or
/// keeping it awake: the person asked for these to be left alone, and
/// with them every process above them up to the agent.
pub fn argv(argv: &[String]) -> bool {
    argv.iter().any(|a| a.contains("bench-lock") || a.contains("caffeinate"))
}

/// marspot's own processes and codex's servers.
pub fn comm(comm: &str, argv: Option<&[String]>) -> bool {
    comm.starts_with("marspot")
        || comm == "codex-code-mode-host"
        || (comm == "codex" && argv.is_some_and(|a| a.get(1).is_some_and(|s| s.starts_with("app-server"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_exclusions_keep_system_and_app_processes_but_not_developer_tools() {
        assert!(exe(Path::new("/System/Library/CoreServices/x")));
        assert!(exe(Path::new("/usr/libexec/logd")));
        assert!(exe(Path::new("/Applications/wechatwebdevtools.app/Contents/MacOS/Electron")));
        let helper = "/Applications/w.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler";
        assert!(exe(Path::new(helper)));
        assert!(!exe(Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/simctl")));
        assert!(!exe(Path::new("/usr/bin/tail")));
    }

    #[test]
    fn zombie_exclusions_keep_the_measurement_lock_and_codex_servers() {
        let a = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(argv(&a("/bin/zsh -c bench-lock heavy cargo test")));
        assert!(argv(&a("caffeinate -i -t 300")));
        assert!(!argv(&a("bash watchdog.sh --stopped 3600")));
        assert!(comm("marspot-peer", None));
        assert!(comm("codex", Some(&a("codex app-server --listen unix://"))));
        assert!(comm("codex-code-mode-host", None));
        assert!(!comm("codex", Some(&a("codex exec ls"))));
    }
}
