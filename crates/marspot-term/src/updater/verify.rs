//! Which keys a release may be signed with, and the check itself.

use std::path::Path;
use std::process::Command;

use super::cache_dir;

/// The release-signing public key, checked in at
/// `keys/marspot-update.pub` and baked into the binary so the trust
/// anchor travels with the code instead of the filesystem.
pub(super) const UPDATE_PUBKEY_PEM: &str = include_str!("../../../../keys/marspot-update.pub");

/// Where a build with the `test-update-key` feature reads its second
/// trusted key from.
#[cfg(any(test, feature = "test-update-key"))]
pub(super) const ENV_TEST_UPDATE_PUBKEY: &str = "MARSPOT_TEST_UPDATE_PUBKEY";

/// The public keys a release may be signed with: the embedded one, and
/// in a `test-update-key` build running in a sandbox, the PEM file
/// `MARSPOT_TEST_UPDATE_PUBKEY` names.  Outside a sandbox the variable
/// is ignored, so a test build that escapes onto the real state dir
/// still trusts only the real key.
pub(super) fn trusted_pubkeys() -> Vec<String> {
    let embedded = UPDATE_PUBKEY_PEM.to_string();
    #[cfg(any(test, feature = "test-update-key"))]
    if std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty())
        && let Some(path) = std::env::var_os(ENV_TEST_UPDATE_PUBKEY)
        && let Ok(pem) = std::fs::read_to_string(path)
    {
        return vec![embedded, pem];
    }
    vec![embedded]
}

/// Verify `file` against its detached `sig` using the trusted public
/// keys; any one of them verifying is enough.  ECDSA P-256 / SHA-256
/// via `/usr/bin/openssl dgst` (LibreSSL — see the module doc for why
/// not Ed25519).
pub(super) fn verify_signature(file: &Path, sig: &Path) -> Result<(), String> {
    let dir = cache_dir().join("download");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir pubkey dir: {}", e))?;
    let mut last = String::from("no trusted key");
    for (i, pem) in trusted_pubkeys().iter().enumerate() {
        let name = if i == 0 {
            "marspot-update.pub".to_string()
        } else {
            format!("marspot-update.{i}.pub")
        };
        let pubkey = dir.join(name);
        std::fs::write(&pubkey, pem).map_err(|e| format!("write pubkey: {}", e))?;
        match verify_signature_with(&pubkey, file, sig) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Inner verify, parameterised on the public-key path so tests can
/// run against a throwaway keypair.
pub(super) fn verify_signature_with(pubkey: &Path, file: &Path, sig: &Path) -> Result<(), String> {
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

/// A throwaway P-256 keypair, made with the same `/usr/bin/openssl`
/// the updater verifies with.
#[cfg(test)]
pub(super) mod testkey {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn openssl(args: &[&std::ffi::OsStr]) {
        let out = Command::new("/usr/bin/openssl").args(args).output().unwrap();
        assert!(out.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// `(secret, public)` key files in `dir`, named after `tag`.
    pub fn keypair(dir: &Path, tag: &str) -> (PathBuf, PathBuf) {
        let sec = dir.join(format!("{tag}.sec"));
        let public = dir.join(format!("{tag}.pub"));
        openssl(&[
            "ecparam".as_ref(),
            "-genkey".as_ref(),
            "-name".as_ref(),
            "prime256v1".as_ref(),
            "-noout".as_ref(),
            "-out".as_ref(),
            sec.as_os_str(),
        ]);
        openssl(&[
            "ec".as_ref(),
            "-in".as_ref(),
            sec.as_os_str(),
            "-pubout".as_ref(),
            "-out".as_ref(),
            public.as_os_str(),
        ]);
        (sec, public)
    }

    pub fn sign(sec: &Path, file: &Path, sig: &Path) {
        openssl(&[
            "dgst".as_ref(),
            "-sha256".as_ref(),
            "-sign".as_ref(),
            sec.as_os_str(),
            "-out".as_ref(),
            sig.as_os_str(),
            file.as_os_str(),
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::testkey::{keypair, sign};
    use super::*;
    use crate::test_env::{EnvGuard, TempDir, sandbox_state_dir};

    /// Outside a sandbox the test key is not trusted, whatever the
    /// variable says: a test build pointed at the real state dir must
    /// still accept only the real key.
    #[test]
    fn outside_a_sandbox_only_the_embedded_key_is_trusted() {
        let dir = TempDir::new("update-key");
        let (_, public) = keypair(dir.path(), "test");
        let mut env = EnvGuard::new();
        env.set(ENV_TEST_UPDATE_PUBKEY, &public);
        env.remove("MARSPOT_STATE_DIR");
        assert_eq!(trusted_pubkeys(), vec![UPDATE_PUBKEY_PEM.to_string()]);
        env.set("MARSPOT_STATE_DIR", "");
        assert_eq!(
            trusted_pubkeys(),
            vec![UPDATE_PUBKEY_PEM.to_string()],
            "an empty state dir is not a sandbox"
        );
    }

    /// In a sandbox the named key is added, after the embedded one; no
    /// variable, or a variable naming nothing, adds nothing.
    #[test]
    fn in_a_sandbox_the_named_key_is_trusted_too() {
        let mut sandbox = sandbox_state_dir();
        let (_, public) = keypair(sandbox.path(), "test");
        sandbox.env.remove(ENV_TEST_UPDATE_PUBKEY);
        assert_eq!(trusted_pubkeys(), vec![UPDATE_PUBKEY_PEM.to_string()]);
        sandbox.env.set(ENV_TEST_UPDATE_PUBKEY, sandbox.path().join("absent.pub"));
        assert_eq!(trusted_pubkeys(), vec![UPDATE_PUBKEY_PEM.to_string()]);
        sandbox.env.set(ENV_TEST_UPDATE_PUBKEY, &public);
        let pem = std::fs::read_to_string(&public).unwrap();
        assert_eq!(trusted_pubkeys(), vec![UPDATE_PUBKEY_PEM.to_string(), pem]);
    }

    /// The whole check: a file signed with the test key verifies only
    /// while the key is trusted, and one signed with any other key never
    /// does.
    #[test]
    fn a_signature_verifies_only_against_a_trusted_key() {
        let mut sandbox = sandbox_state_dir();
        let dir = sandbox.path().to_path_buf();
        let (sec, public) = keypair(&dir, "test");
        let (other_sec, _) = keypair(&dir, "other");
        let file = dir.join("payload");
        std::fs::write(&file, b"release bytes").unwrap();
        let sig = dir.join("payload.sig");
        let other_sig = dir.join("payload.other.sig");
        sign(&sec, &file, &sig);
        sign(&other_sec, &file, &other_sig);

        sandbox.env.remove(ENV_TEST_UPDATE_PUBKEY);
        assert!(verify_signature(&file, &sig).is_err(), "the test key is not trusted yet");
        sandbox.env.set(ENV_TEST_UPDATE_PUBKEY, &public);
        assert!(verify_signature(&file, &sig).is_ok());
        assert!(verify_signature(&file, &other_sig).is_err(), "an untrusted key");
        sandbox.env.remove("MARSPOT_STATE_DIR");
        // cache_dir() follows the state dir, so the check now writes
        // into the default state root; point HOME away from the user's
        let home = TempDir::new("update-key-home");
        sandbox.env.set("HOME", home.path());
        assert!(
            verify_signature(&file, &sig).is_err(),
            "out of the sandbox the test key is not trusted"
        );
    }
}
