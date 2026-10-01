//! Where a pane's submit-receipt token comes from, shared by the two
//! layers that need it.
//!
//! L1 listens for receipts; L3 spawns the pane and must put the token
//! in its environment. They are different processes in different
//! crates, so the token cannot simply be passed -- and threading a
//! secret down L1 → L2 → L3 would add a field to two wire hops for a
//! value neither of them has any other use for.
//!
//! They already share one thing: the state root. So L1 writes a secret
//! there once, and both sides derive the same token from it. A pane's
//! token is a function of (secret, session id), which gives the
//! property that matters -- a token names exactly one pane, so a
//! leaked one cannot be used to speak for another.
//!
//! The secret is not protection against someone who can already read
//! the state dir; at that point they can read the socket too. It is
//! there so that a token cannot be *guessed* from a session id, which
//! is a number any pane knows.

use std::io::Write;
use std::path::PathBuf;

/// The variable carrying a pane's token into its environment.
pub const TOKEN_VAR: &str = "MARSPOT_SUBMIT_TOKEN";

pub fn socket_path() -> PathBuf {
    crate::paths::state_root().join("submit-receipts.sock")
}

fn secret_path() -> PathBuf {
    crate::paths::state_root().join("submit-receipt.key")
}

/// Read the secret.  **Never creates anything.**
///
/// `token_for` is called while a pane is being spawned, and that path
/// has to stay free of filesystem side effects. The first version
/// created the secret here, which meant every pane spawn could
/// `create_dir_all` the state root -- and two `local_session` tests
/// stopped seeing their shell come up at all. Whatever the mechanism
/// was, a spawn path that writes to disk to answer a question is the
/// wrong shape; the tests were right to object.
///
/// L1 calls [`ensure_secret`] once at startup. A pane that finds no
/// secret has no token, which costs it receipts and nothing else.
fn read_secret() -> Option<String> {
    let s = std::fs::read_to_string(secret_path()).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Create the secret if it is not there.  L1, once, at startup.
///
/// `None` when the state dir cannot be written, which is not fatal:
/// receipts are an improvement on reading the pane, and the pane is
/// still there to read.
pub fn ensure_secret() -> Option<String> {
    if let Some(s) = read_secret() {
        return Some(s);
    }
    let path = secret_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let fresh = format!("{:016x}{:016x}", random_u64(), random_u64());
    // 0600 and O_EXCL: two panes starting at once must not each write
    // a different secret, or the one that loses hands out tokens the
    // listener will not recognise.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(fresh.as_bytes()).ok()?;
            Some(fresh)
        }
        // Somebody else just created it — theirs is the real one.
        Err(_) => std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()),
    }
}

use std::os::unix::fs::OpenOptionsExt as _;

/// The token for `sid`.  Both sides compute it; neither sends it.
pub fn token_for(sid: u64) -> Option<String> {
    let secret = read_secret()?;
    Some(format!("{sid:x}-{:016x}", mix(&secret, sid)))
}

/// Does `token` belong to `sid`?
pub fn token_names(sid: u64, token: &str) -> bool {
    token_for(sid).is_some_and(|t| t == token)
}

/// What a submitted line is recognised by.
///
/// Both sides compute it, so it lives in one place. The ends are
/// trimmed: a composer is free to tidy what it was handed, and a
/// receipt differing only by a trailing newline is the same submit.
pub fn fingerprint(text: &str) -> u64 {
    use std::hash::Hasher as _;
    let mut h = crate::fast_hash::FxHasher::default();
    h.write(text.trim().as_bytes());
    h.finish()
}

fn mix(secret: &str, sid: u64) -> u64 {
    use std::hash::Hasher as _;
    let mut h = crate::fast_hash::FxHasher::default();
    h.write(secret.as_bytes());
    h.write_u64(sid);
    h.finish()
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token names one pane.  This is the property the whole scheme
    /// rests on: a token that leaked out of one pane must not let it
    /// report submits for another.
    #[test]
    fn a_token_names_exactly_one_pane() {
        let dir = std::env::temp_dir().join(format!("marspot-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: test code, single-threaded at this point.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };

        assert_eq!(token_for(7), None, "no secret yet, so no token");
        ensure_secret().expect("a secret");
        let a = token_for(7).expect("a token");
        let b = token_for(8).expect("a token");
        assert_ne!(a, b, "two panes, two tokens");
        assert!(token_names(7, &a));
        assert!(!token_names(8, &a), "pane 8 cannot answer to pane 7's token");
        assert_eq!(token_for(7).as_deref(), Some(a.as_str()), "stable across calls");

        // Not guessable from the session id alone: a fresh secret has
        // to produce a different token for the same pane.
        let _ = std::fs::remove_dir_all(&dir);
        ensure_secret().expect("a secret");
        let c = token_for(7).expect("a token");
        assert_ne!(a, c, "a new secret means new tokens");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fingerprint_ignores_the_edges() {
        assert_eq!(fingerprint("carry on"), fingerprint("  carry on\n"));
        assert_ne!(fingerprint("carry on"), fingerprint("carry on now"));
    }
}
