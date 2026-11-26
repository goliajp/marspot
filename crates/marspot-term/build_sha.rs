// The commit a binary names in its fingerprint (`MARSPOT_FP`), shared by
// the root crate's build.rs, this crate's build.rs and the integration
// tests (each includes it by path).  A runner builds from a synced copy
// with no `.git`, so the syncing side passes its sha in `MARSPOT_GIT_SHA`;
// a working repository always wins over that variable.

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

#[cfg(test)]
mod tests {
    use super::choose;

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
}
