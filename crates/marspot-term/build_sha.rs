// The commit a binary names in its fingerprint (`MARSPOT_FP`), shared by
// the root crate's build.rs, this crate's build.rs and the integration
// tests (each includes it by path).  A runner builds from a synced copy
// with no `.git`, so the syncing side passes its sha in `MARSPOT_GIT_SHA`;
// a working repository always wins over that variable.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `git_head` is `git rev-parse --short=8 HEAD` (None when there is no
/// repository), `git_dirty` whether `git status --porcelain` printed
/// anything, `env` the value of `MARSPOT_GIT_SHA`
pub fn choose(git_head: Option<&str>, git_dirty: bool, env: Option<&str>) -> String {
    if let Some(head) = git_head {
        return if git_dirty { format!("{head}-dirty") } else { head.to_string() };
    }
    env.and_then(from_env).unwrap_or_else(|| "unknown".to_string())
}

// accepts `^[0-9a-f]{7,40}(-dirty)?$` and shortens the hex to the 8
// characters `rev-parse --short=8` gives
fn from_env(value: &str) -> Option<String> {
    let (hex, dirty) = match value.strip_suffix("-dirty") {
        Some(hex) => (hex, true),
        None => (value, false),
    };
    let is_hex = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !(7..=40).contains(&hex.len()) || !is_hex {
        return None;
    }
    let short = &hex[..hex.len().min(8)];
    Some(if dirty { format!("{short}-dirty") } else { short.to_string() })
}

/// The git files whose change can move what `choose` returns, for
/// `cargo:rerun-if-changed`: HEAD and the index from the worktree's own
/// git dir, packed-refs and HEAD's branch ref from the common dir.  In
/// a linked worktree `.git` is a file and the two dirs differ, so the
/// paths come from `git rev-parse`, not from `.git/`.  Only paths that
/// exist: cargo takes a missing one as forever stale and reruns the
/// script, and with it the new build time and every crate after it, on
/// each build.  Empty when `dir` is in no repository.
pub fn git_watch_paths(dir: &Path) -> Vec<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"])
        .args(["--symbolic-full-name", "HEAD"])
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else { return Vec::new() };
    let (git_dir, common) = (Path::new(git_dir), Path::new(common));
    let mut paths = vec![git_dir.join("HEAD"), git_dir.join("index"), common.join("packed-refs")];
    // a detached HEAD prints `HEAD` here and has no branch ref to watch
    if let Some(refname) = lines.next().filter(|r| r.starts_with("refs/")) {
        paths.push(common.join(refname));
    }
    paths.retain(|p| p.exists());
    paths
}

/// What the build scripts print for `git_watch_paths`.
pub fn print_git_watch() {
    for p in git_watch_paths(Path::new(".")) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::{choose, git_watch_paths};

    #[test]
    fn git_wins_over_the_variable() {
        assert_eq!(choose(Some("0cd671d9"), false, Some("aaaaaaaa")), "0cd671d9");
        assert_eq!(choose(Some("0cd671d9"), true, Some("aaaaaaaa")), "0cd671d9-dirty");
        assert_eq!(choose(Some("0cd671d9"), false, Some("nonsense")), "0cd671d9");
    }

    #[test]
    fn without_a_repository_the_variable_is_used() {
        assert_eq!(choose(None, false, Some("0cd671d9")), "0cd671d9");
        assert_eq!(choose(None, false, Some("0cd671d9-dirty")), "0cd671d9-dirty");
        // the dirty flag belongs to the repository, not to the variable
        assert_eq!(choose(None, true, Some("0cd671d9")), "0cd671d9");
    }

    #[test]
    fn a_long_sha_is_shortened_to_eight() {
        let full = "f3c75b2012345678901234567890123456789012";
        assert_eq!(choose(None, false, Some(full)), "f3c75b20");
        assert_eq!(choose(None, false, Some(&format!("{full}-dirty"))), "f3c75b20-dirty");
        assert_eq!(choose(None, false, Some("f3c75b2")), "f3c75b2");
    }

    #[test]
    fn anything_else_is_unknown() {
        for bad in [
            "",
            "-dirty",
            "f3c75b",
            "F3C75B20",
            "f3c75b2g",
            "f3c75b20-clean",
            "f3c75b20-dirty-dirty",
            " f3c75b20",
            "f3c75b20\n",
            "f3c75b2012345678901234567890123456789012a",
        ] {
            assert_eq!(choose(None, false, Some(bad)), "unknown", "{bad:?}");
        }
        assert_eq!(choose(None, false, None), "unknown");
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A linked worktree has a `.git` file, not a directory: its HEAD and
    /// index are under the main repository's `worktrees/<name>`, its
    /// branch ref under the main repository's `refs/`.
    #[test]
    fn a_linked_worktree_watches_its_own_git_files() {
        let tmp = marspot_term::test_env::TempDir::new("build-sha-worktree");
        let main = tmp.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "trunk"]);
        std::fs::write(main.join("f"), "x").unwrap();
        git(&main, &["add", "f"]);
        git(&main, &["commit", "-q", "-m", "x"]);
        let linked = tmp.join("linked");
        git(&main, &["worktree", "add", "-q", "-b", "side", linked.to_str().unwrap()]);
        assert!(linked.join(".git").is_file());

        let common = main.join(".git").canonicalize().unwrap();
        let own = common.join("worktrees/linked");
        let watched: Vec<_> = git_watch_paths(&linked).into_iter().map(|p| p.canonicalize().unwrap()).collect();
        assert_eq!(watched, [own.join("HEAD"), own.join("index"), common.join("refs/heads/side")]);

        let watched: Vec<_> = git_watch_paths(&main).into_iter().map(|p| p.canonicalize().unwrap()).collect();
        assert_eq!(watched, [common.join("HEAD"), common.join("index"), common.join("refs/heads/trunk")]);

        git(&main, &["pack-refs", "--all"]);
        git(&linked, &["checkout", "-q", "--detach"]);
        let watched: Vec<_> = git_watch_paths(&linked).into_iter().map(|p| p.canonicalize().unwrap()).collect();
        assert_eq!(watched, [own.join("HEAD"), own.join("index"), common.join("packed-refs")]);
    }
}
