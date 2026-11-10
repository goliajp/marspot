//! What a pane's shell is handed on top of the environment it
//! inherits.
//!
//! Applied after the `MARSPOT_` sweep, so what is minted here for this
//! pane survives while every inherited name of that kind still dies.

/// The variables set in pane `id`'s shell.
///
/// The submit-receipt token: L1 derives the same one from the same
/// secret, so neither layer has to send it to the other.  A pane with
/// no token has no receipts, and the submit path reads the screen
/// instead.
///
/// `PATH` with `~/.local/bin` on it: that is where the agent CLIs are
/// installed, and an app started by launchd does not have it.
pub(crate) fn for_pane(id: u64) -> Vec<(String, String)> {
    let mut set = Vec::new();
    if let Some(t) = marspot_term::submit_receipt::token_for(id) {
        set.push((marspot_term::submit_receipt::TOKEN_VAR.to_string(), t));
    }
    if let Some(home) = std::env::var_os("HOME") {
        set.push(("PATH".to_string(), path(std::env::var("PATH").ok().as_deref(), std::path::Path::new(&home))));
    }
    set
}

fn path(inherited: Option<&str>, home: &std::path::Path) -> String {
    marspot_term::paths::path_with_local_bin(inherited.unwrap_or(marspot_term::paths::DEFAULT_PATH), home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_pane_path_gains_local_bin_once_and_in_front() {
        let home = Path::new("/Users/x");
        assert_eq!(path(Some("/usr/bin:/bin"), home), "/Users/x/.local/bin:/usr/bin:/bin");
        assert_eq!(path(Some("/usr/bin:/Users/x/.local/bin:/bin"), home), "/usr/bin:/Users/x/.local/bin:/bin");
        assert_eq!(path(None, home), "/Users/x/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    }
}
