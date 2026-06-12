//! Silent-update background poller.
//!
//! Once per launch + every `POLL_INTERVAL` afterward, polls the
//! GitHub Releases API for the project's latest tag.  If newer than
//! `CARGO_PKG_VERSION`, downloads the matching asset plus its
//! detached `.sig`, verifies the signature against the public key
//! compiled into this binary, and stages each binary into the
//! supervisor's pending slot under
//! `~/Library/Caches/marspot/binaries/pending/`.
//!
//! `marspot-shell` is the supervisor — it reads that slot, atomic-swaps
//! `current ← pending`, kills the running core, exec's the new one,
//! and watches probation.  See `bin/marspot-shell/supervisor.rs` for
//! the swap state machine.  The updater here only owns the
//! "download + verify + stage" half.
//!
//! Self-build constraints: HTTP via `/usr/bin/curl`, JSON parsed with
//! a minimal hand-written scanner (only two fields from a known
//! endpoint), signatures verified via `/usr/bin/openssl dgst`.  No
//! new Rust crates; we depend on system tools that ship with every
//! macOS since well before our minimum target.
//!
//! Trust model (v1.1): a release tarball must carry a detached
//! signature made with `keys/marspot-update.sec` (offline / GH
//! secret); the public half is checked in at
//! `keys/marspot-update.pub` and embedded here at compile time, so
//! a compromised GitHub account or CDN can't push runnable binaries.
//! The algorithm is ECDSA P-256 over SHA-256 rather than the
//! Ed25519 originally sketched: macOS's stock `/usr/bin/openssl` is
//! LibreSSL 3.3, which has no Ed25519 in `genpkey`/`pkeyutl` —
//! P-256 + `dgst` is the strongest scheme every supported macOS can
//! verify with system tools alone.  Releases missing a `.sig` asset
//! are rejected outright.

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
    // Any one of the three binaries having a pending entry counts —
    // the shell will pick them up next focus-loss / SIGUSR1.
    STAGED_BINARIES.iter().any(|b| pending_binary_path(b).exists())
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/marspot")
}

fn pending_binary_path(bin_name: &str) -> PathBuf {
    // Step 5+ / Task A: lives under `binaries/pending/<bin_name>` so
    // the supervisor (marspot-shell) finds it via `BinaryTree::pending()`.
    cache_dir().join("binaries/pending").join(bin_name)
}

/// All three binaries the release tarball is expected to ship.
/// Order matters only for the log: core is the safest to apply (no
/// session loss), shell is next (window flash), shelld last (kills
/// sessions — gated behind explicit `bin/install-shelld.sh
/// --apply-pending`).
const STAGED_BINARIES: &[&str] = &[
    "marspot-core",
    "marspot-shell",
    "marspot-shelld",
];

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
    let sig_name = format!("{}.sig", asset_name);
    let sig_url = scrape_asset_url(body, &sig_name).ok_or_else(|| {
        format!(
            "feed has no signature asset {} for tag {} — unsigned releases are rejected",
            sig_name, tag
        )
    })?;

    // Download to temp files, then verify, then stage.  If verify
    // fails the staged path never gets written, so the supervisor
    // never sees a bad binary.
    let tmp = cache_dir().join("download/marspot.tar.gz");
    let tmp_sig = cache_dir().join("download/marspot.tar.gz.sig");
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir tmp: {}", e))?;
    }
    download_to(&asset_url, &tmp)?;
    download_to(&sig_url, &tmp_sig)?;
    if let Err(e) = verify_signature(&tmp, &tmp_sig) {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&tmp_sig);
        return Err(format!("signature verification failed: {}", e));
    }
    let _ = std::fs::remove_file(&tmp_sig);
    // Extract each layer's binary out of the tarball into its own
    // pending slot.  `extract_binary` is named-lookup, so a tarball
    // with only `marspot-core` (legacy single-binary releases) still
    // works — the shell/shelld extracts will silently no-op when the
    // binary isn't in the archive.
    let mut staged_any = false;
    for bin in STAGED_BINARIES {
        let pending = pending_binary_path(bin);
        if let Some(parent) = pending.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir pending: {}", e))?;
        }
        match extract_binary(&tmp, bin, &pending) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&pending)
                    .map_err(|e| format!("stat pending {}: {}", bin, e))?
                    .permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&pending, perms)
                    .map_err(|e| format!("chmod pending {}: {}", bin, e))?;
                strip_quarantine_xattrs(&pending);
                staged_any = true;
            }
            Err(e) => {
                eprintln!("[updater] tarball had no {bin}: {e} — skipping");
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    if !staged_any {
        return Err("tarball contained none of the expected binaries".to_string());
    }
    Ok(true)
}

/// Strip macOS Gatekeeper / quarantine extended attributes from a
/// freshly-staged binary.  Without this, the first launch of a
/// binary that didn't come from the App Store or a notarised DMG
/// stalls in `_dyld_start` for ~30-60 s while LaunchServices
/// runs a synchronous provenance check — fatal for a "silent"
/// upgrade because the new core / shell appears hung.
fn strip_quarantine_xattrs(path: &Path) {
    for attr in &["com.apple.quarantine", "com.apple.provenance"] {
        let _ = Command::new("/usr/bin/xattr")
            .args(["-d", attr])
            .arg(path)
            .output();
    }
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

/// The release-signing public key, checked in at
/// `keys/marspot-update.pub` and baked into the binary so the trust
/// anchor travels with the code instead of the filesystem.
const UPDATE_PUBKEY_PEM: &str = include_str!("../keys/marspot-update.pub");

/// Verify `file` against its detached `sig` using the embedded
/// public key.  ECDSA P-256 / SHA-256 via `/usr/bin/openssl dgst`
/// (LibreSSL — see the module doc for why not Ed25519).
fn verify_signature(file: &Path, sig: &Path) -> Result<(), String> {
    let pubkey = cache_dir().join("download/marspot-update.pub");
    if let Some(parent) = pubkey.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir pubkey dir: {}", e))?;
    }
    std::fs::write(&pubkey, UPDATE_PUBKEY_PEM)
        .map_err(|e| format!("write pubkey: {}", e))?;
    verify_signature_with(&pubkey, file, sig)
}

/// Inner verify, parameterised on the public-key path so tests can
/// run against a throwaway keypair.
fn verify_signature_with(pubkey: &Path, file: &Path, sig: &Path) -> Result<(), String> {
    let out = Command::new("/usr/bin/openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg("-verify")
        .arg(pubkey)
        .arg("-signature")
        .arg(sig)
        .arg(file)
        .output()
        .map_err(|e| format!("spawn openssl: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "openssl dgst -verify exited {}: {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ))
    }
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

/// Extract a named binary out of the downloaded tarball into `dst`.
/// We don't care where in the archive it lives — `find_named_file`
/// recurses.  A v1-style tarball (only `marspot-core`) returns an
/// error for the shell + shelld lookups; callers treat that as a
/// soft skip.
fn extract_binary(archive: &Path, bin_name: &str, dst: &Path) -> Result<(), String> {
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
    let bin = find_named_file(&extract_dir, bin_name).ok_or_else(|| {
        format!("extracted tar contained no '{}' binary", bin_name)
    })?;
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
    fn embedded_pubkey_is_pem() {
        assert!(UPDATE_PUBKEY_PEM.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(UPDATE_PUBKEY_PEM.trim_end().ends_with("-----END PUBLIC KEY-----"));
    }

    /// Round-trip against a throwaway P-256 keypair: a good
    /// signature verifies, a tampered file does not.  Exercises the
    /// exact openssl invocation production uses.
    #[test]
    fn signature_verify_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-sigtest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sec = dir.join("test.sec");
        let pubk = dir.join("test.pub");
        let file = dir.join("payload.bin");
        let sig = dir.join("payload.bin.sig");

        let gen = Command::new("/usr/bin/openssl")
            .args(["ecparam", "-genkey", "-name", "prime256v1", "-noout", "-out"])
            .arg(&sec)
            .output()
            .unwrap();
        assert!(gen.status.success(), "genkey failed");
        let pubout = Command::new("/usr/bin/openssl")
            .arg("ec")
            .arg("-in")
            .arg(&sec)
            .arg("-pubout")
            .arg("-out")
            .arg(&pubk)
            .output()
            .unwrap();
        assert!(pubout.status.success(), "pubout failed");

        std::fs::write(&file, b"release payload bytes").unwrap();
        let sign = Command::new("/usr/bin/openssl")
            .arg("dgst")
            .arg("-sha256")
            .arg("-sign")
            .arg(&sec)
            .arg("-out")
            .arg(&sig)
            .arg(&file)
            .output()
            .unwrap();
        assert!(sign.status.success(), "sign failed");

        assert!(verify_signature_with(&pubk, &file, &sig).is_ok());

        // Tamper → must fail.
        std::fs::write(&file, b"release payload bytes, but evil").unwrap();
        assert!(verify_signature_with(&pubk, &file, &sig).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
