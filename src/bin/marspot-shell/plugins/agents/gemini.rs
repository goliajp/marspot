//! gemini, from the standalone builds on its GitHub releases.
//!
//! The usual install is an npm package, and the machine may have no
//! Node.  A release carries one self-contained executable per
//! platform, unsigned; Apple silicon kills an unsigned executable as it
//! starts, so it is signed ad hoc here before anything runs it.
//!
//! The release tag of what was installed is kept in the state root.
//! That file is also what says the gemini on this machine is marspot's
//! to replace: one that came from npm or brew has no tag, and is never
//! touched.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use marspot_peer::json::{self, Value};

use super::{fetch, run, scratch, version, Places};

const LATEST: &str = "https://api.github.com/repos/google-gemini/gemini-cli/releases/latest";
/// Where a release's own files are served from; an asset that points
/// anywhere else is not downloaded.
const DOWNLOADS: &str = "https://github.com/google-gemini/gemini-cli/releases/download/";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Release {
    pub tag: String,
    pub url: String,
}

/// The release asset for a machine of `arch` (`std::env::consts::ARCH`).
pub(super) fn asset_name(arch: &str) -> Option<&'static str> {
    match arch {
        "aarch64" => Some("gemini-darwin-arm64-unsigned.zip"),
        "x86_64" => Some("gemini-darwin-x64-unsigned.zip"),
        _ => None,
    }
}

/// The tag and the download of `asset` out of GitHub's description of
/// a release.
pub(super) fn release_asset(body: &str, asset: &str) -> Result<Release, String> {
    let v = json::parse(body).map_err(|e| format!("release: {e}"))?;
    // the tag ends up in a file and a log line
    let tag = v
        .str_at("tag_name")
        .filter(|t| !t.is_empty() && t.len() <= 128 && t.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c)))
        .ok_or("release: no usable tag_name")?;
    let url = v
        .get("assets")
        .map_or(&[][..], Value::arr)
        .iter()
        .find(|a| a.str_at("name") == Some(asset))
        .and_then(|a| a.str_at("browser_download_url"))
        .ok_or_else(|| format!("release {tag} has no {asset}"))?;
    if !url.starts_with(DOWNLOADS) {
        return Err(format!("release {tag}: {asset} is served from somewhere else"));
    }
    Ok(Release { tag: tag.to_string(), url: url.to_string() })
}

/// Whether to install `latest`: always when there is no gemini, and
/// over an existing one only when marspot installed it (`ours` is the
/// tag it recorded) and that was another release.
pub(super) fn should_install(present: bool, ours: Option<&str>, latest: &str) -> bool {
    !present || ours.is_some_and(|tag| tag != latest)
}

fn tag_file(state: &Path) -> PathBuf {
    state.join("agents").join("gemini.tag")
}

pub(super) fn recorded_tag(state: &Path) -> Option<String> {
    std::fs::read_to_string(tag_file(state)).ok().map(|t| t.trim().to_string())
}

/// Install or update when there is something to do; the version
/// installed, or None when there was not.
pub(super) fn step(at: &Places, present: bool) -> Result<Option<String>, String> {
    let ours = recorded_tag(&at.state);
    // somebody else's gemini: not even the release is looked up
    if present && ours.is_none() {
        return Ok(None);
    }
    let asset = asset_name(std::env::consts::ARCH).ok_or("no gemini build for this machine")?;
    let body = run(
        Command::new("/usr/bin/curl")
            .args(["-fsSL", "--max-time", "60", "-H", "accept: application/vnd.github+json", "-H", "user-agent: marspot"])
            .arg(LATEST),
        "curl",
    )?;
    let release = release_asset(&String::from_utf8_lossy(&body), asset)?;
    if !should_install(present, ours.as_deref(), &release.tag) {
        return Ok(None);
    }
    let version = install(&release.url, &at.home)?;
    let file = tag_file(&at.state);
    std::fs::create_dir_all(at.state.join("agents"))
        .and_then(|()| std::fs::write(&file, format!("{}\n", release.tag)))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(version))
}

fn install(url: &str, home: &Path) -> Result<String, String> {
    let dir = scratch("gemini")?;
    let r = (|| {
        let zip = dir.join("gemini.zip");
        fetch(url, &zip)?;
        let unpacked = dir.join("unpacked");
        run(Command::new("/usr/bin/unzip").args(["-o", "-q"]).arg(&zip).arg("-d").arg(&unpacked), "unzip")?;
        place(&single_executable(&unpacked)?, &home.join(".local/bin"))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// The one file a release archive holds.
pub(super) fn single_executable(unpacked: &Path) -> Result<PathBuf, String> {
    let names: Vec<String> = std::fs::read_dir(unpacked)
        .map_err(|e| format!("{}: {e}", unpacked.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let file = unpacked.join("gemini");
    let executable =
        std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if names != ["gemini"] || !executable {
        return Err(format!("the archive is not one executable called gemini: {}", names.join(" ")));
    }
    Ok(file)
}

/// Sign `unpacked`, see that it runs, and make it `<bin_dir>/gemini`.
///
/// Staged in a directory beside the destination so the last step is a
/// rename within one file system: the command is the old executable
/// or the new one and never half of either, and it arrives as a new
/// inode, which matters because the system caches a signature per
/// inode.  A build that does not run is dropped and the old command
/// stays.
fn place(unpacked: &Path, bin_dir: &Path) -> Result<String, String> {
    let stage = bin_dir.join(format!(".marspot-gemini-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let staged = stage.join("gemini");
    let r = (|| {
        std::fs::copy(unpacked, &staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        run(Command::new("/usr/bin/codesign").args(["-s", "-", "-f"]).arg(&staged), "codesign")?;
        let version = version(&staged)?;
        let dest = bin_dir.join("gemini");
        std::fs::rename(&staged, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        Ok(version)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = r#"{
      "url": "https://api.github.com/repos/google-gemini/gemini-cli/releases/1",
      "tag_name": "v0.67.0-nightly.20261109.a1b2c3d",
      "prerelease": false,
      "assets": [
        {"name": "gemini-darwin-x64-unsigned.zip", "size": 1,
         "browser_download_url": "https://github.com/google-gemini/gemini-cli/releases/download/v0.67.0-nightly.20261109.a1b2c3d/gemini-darwin-x64-unsigned.zip"},
        {"name": "gemini-darwin-arm64-unsigned.zip", "size": 2,
         "browser_download_url": "https://github.com/google-gemini/gemini-cli/releases/download/v0.67.0-nightly.20261109.a1b2c3d/gemini-darwin-arm64-unsigned.zip"},
        {"name": "gemini-linux-x64.zip", "browser_download_url": "https://github.com/google-gemini/gemini-cli/releases/download/v0.67.0-nightly.20261109.a1b2c3d/gemini-linux-x64.zip"}
      ]
    }"#;

    #[test]
    fn agents_gemini_release_names_the_tag_and_this_machines_asset() {
        let arm = release_asset(RELEASE, asset_name("aarch64").unwrap()).unwrap();
        assert_eq!(arm.tag, "v0.67.0-nightly.20261109.a1b2c3d");
        assert_eq!(
            arm.url,
            "https://github.com/google-gemini/gemini-cli/releases/download/v0.67.0-nightly.20261109.a1b2c3d/gemini-darwin-arm64-unsigned.zip"
        );
        let intel = release_asset(RELEASE, asset_name("x86_64").unwrap()).unwrap();
        assert!(intel.url.ends_with("/gemini-darwin-x64-unsigned.zip"));
        assert_eq!(asset_name("riscv64"), None);
    }

    #[test]
    fn agents_gemini_release_that_cannot_be_trusted_is_refused() {
        let asset = "gemini-darwin-arm64-unsigned.zip";
        assert!(release_asset("<html>rate limited</html>", asset).is_err());
        assert!(release_asset(r#"{"message": "API rate limit exceeded"}"#, asset).is_err());
        assert!(release_asset(r#"{"tag_name": "v1.0.0", "assets": []}"#, asset).unwrap_err().contains("has no"));
        assert!(release_asset(&RELEASE.replace("v0.67.0-nightly.20261109.a1b2c3d\",", "../../x\","), asset).is_err());
        let elsewhere = RELEASE.replace("https://github.com/google-gemini/", "https://example.com/google-gemini/");
        assert!(release_asset(&elsewhere, asset).unwrap_err().contains("somewhere else"));
    }

    #[test]
    fn agents_gemini_is_replaced_only_when_marspot_installed_it() {
        // none at all: install
        assert!(should_install(false, None, "v2"));
        assert!(should_install(false, Some("v2"), "v2"));
        // ours and another release: update; ours and the same: nothing
        assert!(should_install(true, Some("v1"), "v2"));
        assert!(!should_install(true, Some("v2"), "v2"));
        // no tag recorded: somebody else's, left alone
        assert!(!should_install(true, None, "v2"));
    }

    #[test]
    fn agents_gemini_tag_is_read_from_the_state_root() {
        let state = std::env::temp_dir().join(format!("marspot-agents-tag-{}", marspot_peer::rand::hex(6)));
        assert_eq!(recorded_tag(&state), None);
        std::fs::create_dir_all(state.join("agents")).unwrap();
        std::fs::write(tag_file(&state), "v0.67.0\n").unwrap();
        assert_eq!(recorded_tag(&state).as_deref(), Some("v0.67.0"));
        std::fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn agents_gemini_archive_must_hold_one_executable() {
        let d = std::env::temp_dir().join(format!("marspot-agents-zip-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        assert!(single_executable(&d).is_err(), "empty");
        std::fs::write(d.join("gemini"), "x").unwrap();
        std::fs::set_permissions(d.join("gemini"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(single_executable(&d).is_err(), "not executable");
        std::fs::set_permissions(d.join("gemini"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(single_executable(&d), Ok(d.join("gemini")));
        std::fs::write(d.join("README"), "x").unwrap();
        assert!(single_executable(&d).is_err(), "something else beside it");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
