//! Silent-update background poller.
//!
//! Once per launch + every `POLL_INTERVAL` afterward, polls the
//! GitHub Releases API for the project's latest tag.  If newer than
//! `CARGO_PKG_VERSION`, downloads the matching asset, verifies it
//! against the `digest` field GitHub publishes alongside each asset
//! (SHA-256), and stages the binary into
//! `~/Library/Caches/marspot/pending/marspot`.
//!
//! Bootstrap (Phase 6) picks it up on the next focus-loss trigger
//! and atomically swaps it in.
//!
//! Self-build constraints: HTTP via `/usr/bin/curl`, JSON parsed with
//! a minimal hand-written scanner (only two fields from a known
//! endpoint), SHA-256 verification via `/usr/bin/shasum`.  No new
//! Rust crates; we depend on system tools that ship with every
//! macOS since well before our minimum target.
//!
//! Trust model: HTTPS + GitHub's manifest-published digest is the
//! v1 anchor.  A proper Ed25519 signing chain (minisign-style) is
//! Phase 8 — the architecture here makes it a swap of the verify
//! function, not a rewrite.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long to wait between background polls of the releases API
/// after the initial startup check.  24h matches Chrome's cadence —
/// enough to land hotfixes within a day, slow enough that the user's
/// network isn't constantly bothered.
const POLL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default GitHub Releases endpoint.  Overridable via
/// `MARSPOT_UPDATE_FEED` for dev / staging — the env var should
/// point at a URL with the same JSON shape.
const DEFAULT_FEED: &str =
    "https://api.github.com/repos/goliajp/marspot/releases/latest";

/// Set when the background poller has staged a binary that the
/// foreground process should pick up on next focus loss.  Toggled
/// in the same `Arc<AtomicBool>` the GUI's title-bar affordance
/// reads.
pub type UpdateFlag = Arc<AtomicBool>;

/// Spawn the background updater.  Returns the shared flag so the
/// GUI can light up a "↻ refresh available" title-bar button.
pub fn spawn(version: String) -> UpdateFlag {
    let flag = Arc::new(AtomicBool::new(false));
    if pending_already_staged() {
        flag.store(true, Ordering::Release);
    }
    let flag_thread = flag.clone();
    thread::Builder::new()
        .name("marspot-updater".into())
        .spawn(move || updater_loop(version, flag_thread))
        .expect("spawn updater thread");
    flag
}

fn updater_loop(version: String, flag: UpdateFlag) {
    // Stagger a touch so we don't race the GUI's first paint on
    // startup; 30s is enough for shell prompts to land first.
    thread::sleep(Duration::from_secs(30));
    loop {
        match check_and_stage(&version) {
            Ok(true) => {
                flag.store(true, Ordering::Release);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("[updater] check failed: {}", e);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn pending_already_staged() -> bool {
    pending_binary_path().exists()
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/marspot")
}

fn pending_binary_path() -> PathBuf {
    cache_dir().join("pending/marspot")
}

fn feed_url() -> String {
    std::env::var("MARSPOT_UPDATE_FEED").unwrap_or_else(|_| DEFAULT_FEED.into())
}

/// Top-level orchestration: fetch the feed, decide whether it points
/// to a version newer than the running one, download / verify /
/// stage if so.  Returns `true` when a binary was staged this
/// iteration.
fn check_and_stage(running_version: &str) -> Result<bool, String> {
    let json = http_get(&feed_url(), 8 * 1024 * 1024)?;
    let body = std::str::from_utf8(&json).map_err(|e| format!("non-utf8 feed: {}", e))?;
    let tag = scrape_string(body, "\"tag_name\"")
        .ok_or_else(|| "feed missing tag_name".to_string())?;
    let normalized = tag.trim_start_matches('v').to_string();
    if !is_newer_than(&normalized, running_version) {
        return Ok(false);
    }
    let asset_name = asset_filename();
    let asset_url = scrape_asset_url(body, &asset_name).ok_or_else(|| {
        format!(
            "feed has no asset named {} for tag {}",
            asset_name, tag
        )
    })?;
    let asset_digest = scrape_asset_digest(body, &asset_name);

    // Download to a temp file, then verify, then stage.  If verify
    // fails the staged path never gets written, so the bootstrap
    // never sees a bad binary.
    let tmp = cache_dir().join("download/marspot.tar.gz");
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir tmp: {}", e))?;
    }
    download_to(&asset_url, &tmp)?;
    if let Some(expected) = asset_digest {
        let got = sha256_of(&tmp)?;
        if !digest_matches(&expected, &got) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "SHA-256 mismatch: expected {} got {}",
                expected, got
            ));
        }
    }
    // Extract `marspot` binary out of the tarball into pending/.
    let pending = pending_binary_path();
    if let Some(parent) = pending.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir pending: {}", e))?;
    }
    extract_binary(&tmp, &pending)?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&pending)
        .map_err(|e| format!("stat pending: {}", e))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&pending, perms)
        .map_err(|e| format!("chmod pending: {}", e))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(true)
}

fn asset_filename() -> String {
    // Architecture-tagged so a GitHub Release with both arm64 + x86
    // tarballs hands us the right one.  Only aarch64 is supported
    // today, but the format anticipates a multi-arch release manifest.
    format!("marspot-aarch64-apple-darwin.tar.gz")
}

/// Tiny "parse what I need" scraper.  Given a substring of the
/// shape `"tag_name": "value"`, returns `value`.  Sufficient for the
/// two top-level fields we care about (tag_name, name).  Tolerant of
/// whitespace; respects backslash escapes only enough to detect end
/// of string.
fn scrape_string(body: &str, key: &str) -> Option<String> {
    let i = body.find(key)?;
    let after = &body[i + key.len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    // Skip whitespace
    let after = after.trim_start();
    if !after.starts_with('"') {
        return None;
    }
    let after = &after[1..];
    let mut out = String::new();
    let mut chars = after.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            continue;
        }
        if c == '"' {
            return Some(out);
        }
        out.push(c);
    }
    None
}

/// Locate the asset block matching `asset_name`, then pull its
/// `browser_download_url`.  We scan for `"name": "<asset_name>"`
/// and walk forward from there.
fn scrape_asset_url(body: &str, asset_name: &str) -> Option<String> {
    let needle = format!("\"name\":\"{}\"", asset_name);
    let i = body.replace(' ', "").find(&needle).map(|i| (i, true));
    let pos = i.or_else(|| {
        // Fallback: search with one space variant
        let needle = format!("\"name\": \"{}\"", asset_name);
        body.find(&needle).map(|i| (i, false))
    })?;
    let (start, was_compact) = pos;
    let slice = if was_compact {
        // Re-find in original body to map back accurately.  This
        // path is rare; GitHub returns spaces.
        let i = body.find(asset_name)?;
        &body[i..]
    } else {
        &body[start..]
    };
    scrape_string(slice, "\"browser_download_url\"")
}

/// Same approach but for the `digest` field GitHub introduced for
/// release assets in 2024.  Format is `sha256:<hex>`.  Returns None
/// when the feed doesn't carry one (older API responses, or assets
/// uploaded before the change).
fn scrape_asset_digest(body: &str, asset_name: &str) -> Option<String> {
    let i = body.find(asset_name)?;
    let slice = &body[i..];
    scrape_string(slice, "\"digest\"")
}

fn digest_matches(expected: &str, got: &str) -> bool {
    // Expected format: "sha256:<hex>".  Compare hex part, lowercase.
    let exp_hex = expected.strip_prefix("sha256:").unwrap_or(expected);
    exp_hex.eq_ignore_ascii_case(got)
}

/// Compare semver-shaped strings.  Returns true when `candidate`
/// strictly orders later than `current`.  Strict enough for the
/// `MAJOR.MINOR.PATCH` strings cargo produces; tolerant of an
/// optional `-suffix` for pre-release tags.
fn is_newer_than(candidate: &str, current: &str) -> bool {
    fn split(s: &str) -> (Vec<u64>, bool) {
        let (core, has_pre) = match s.find('-') {
            Some(i) => (&s[..i], true),
            None => (s, false),
        };
        let parts = core
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect();
        (parts, has_pre)
    }
    let (c_parts, c_pre) = split(candidate);
    let (r_parts, r_pre) = split(current);
    let n = c_parts.len().max(r_parts.len());
    for i in 0..n {
        let a = c_parts.get(i).copied().unwrap_or(0);
        let b = r_parts.get(i).copied().unwrap_or(0);
        if a > b {
            return true;
        }
        if a < b {
            return false;
        }
    }
    // Numeric parts equal; semver precedence puts a pre-release tag
    // below its base release (so 0.3.0 > 0.3.0-rc1).
    !c_pre && r_pre
}

/// `curl -sSf -L --max-filesize <cap> <url>`.  Returns the body
/// bytes.  Stdout is collected; non-zero exit code becomes an Err.
fn http_get(url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "-sSfL",
            "--max-time",
            "30",
            "--max-filesize",
            &cap.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: marspot-updater",
            url,
        ])
        .output()
        .map_err(|e| format!("spawn curl: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "curl {} exited {}: {}",
            url,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

fn download_to(url: &str, dst: &Path) -> Result<(), String> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "-sSfL",
            "--max-time",
            "120",
            "--max-filesize",
            "104857600", // 100 MiB hard cap on downloads
            "-H",
            "User-Agent: marspot-updater",
            "-o",
        ])
        .arg(dst)
        .arg(url)
        .output()
        .map_err(|e| format!("spawn curl download: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "curl download {} exited {}: {}",
            url,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|e| format!("spawn shasum: {}", e))?;
    if !out.status.success() {
        return Err(format!("shasum failed: {}", out.status));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let hex = s.split_whitespace().next().unwrap_or("").to_string();
    if hex.is_empty() {
        return Err("shasum returned empty hash".into());
    }
    Ok(hex)
}

/// Extract the `marspot` binary out of the downloaded tarball into
/// `dst`.  We don't care where in the archive it lives — a fresh
/// release tarball ships exactly one such file at the root.
fn extract_binary(archive: &Path, dst: &Path) -> Result<(), String> {
    // Extract to a sibling dir.
    let extract_dir = archive.with_extension("d");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("mkdir extract: {}", e))?;
    let out = Command::new("/usr/bin/tar")
        .args(["-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(&extract_dir)
        .output()
        .map_err(|e| format!("spawn tar: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let bin = find_named_file(&extract_dir, "marspot")
        .ok_or_else(|| "extracted tar contained no 'marspot' binary".to_string())?;
    std::fs::rename(&bin, dst)
        .or_else(|_| std::fs::copy(&bin, dst).map(|_| ()))
        .map_err(|e| format!("stage extracted binary: {}", e))?;
    let _ = std::fs::remove_dir_all(&extract_dir);
    Ok(())
}

fn find_named_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|s| s.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(p) = find_named_file(&path, name) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrape_simple_string_field() {
        let body = r#"{"tag_name": "v1.2.3", "other": "x"}"#;
        assert_eq!(scrape_string(body, "\"tag_name\""), Some("v1.2.3".into()));
    }

    #[test]
    fn scrape_string_handles_escapes() {
        let body = r#"{"name": "with \"quotes\" inside"}"#;
        assert_eq!(
            scrape_string(body, "\"name\""),
            Some("with \"quotes\" inside".into())
        );
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer_than("0.2.1", "0.2.0"));
        assert!(is_newer_than("1.0.0", "0.99.99"));
        assert!(!is_newer_than("0.2.0", "0.2.0"));
        assert!(!is_newer_than("0.1.9", "0.2.0"));
        assert!(is_newer_than("0.3.0", "0.3.0-rc1"));
    }

    #[test]
    fn digest_matches_with_or_without_prefix() {
        let h = "deadbeef".to_string();
        assert!(digest_matches("sha256:DEADBEEF", &h));
        assert!(digest_matches("deadbeef", &h));
        assert!(!digest_matches("sha256:cafe", &h));
    }
}
