//! The account profiles on this machine and the address each one is
//! logged in as.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Scan `$HOME` for `.claude-profile-N` directories and return the
/// profile numbers, sorted ascending.  This is the source of truth
/// for the badge-click cycle — add a `~/.claude-profile-5` dir (plus
/// its `claude5` shell alias) and the cycle picks it up on the next
/// click, no code change.  The default `.claude` (P0) is deliberately
/// not part of the cycle, matching the previous hardcoded 1→2→3 loop.
pub(crate) fn discover_profiles() -> Vec<u8> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    discover_profiles_in(Path::new(&home))
}

pub(super) fn discover_profiles_in(home: &Path) -> Vec<u8> {
    let Ok(rd) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut nums: Vec<u8> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix(".claude-profile-")?
                .parse::<u8>()
                .ok()
        })
        .collect();
    nums.sort_unstable();
    nums
}

/// Read a profile's account address out of its own config.
///
/// The feed is keyed by address, the pane is keyed by directory, and
/// this is the only thing on the machine that ties the two together.
pub(super) fn profile_email(home: &Path, profile: u8) -> Option<String> {
    let body = std::fs::read_to_string(config_of(home, profile)).ok()?;
    let at = body.find("\"oauthAccount\"")?;
    // The file is pretty-printed, so the colon has a space after it —
    // matching `"emailAddress":"` found nothing on the real thing,
    // which is what the live test caught.
    let rest = &body[at..];
    let key = rest.find("\"emailAddress\"")? + "\"emailAddress\"".len();
    let open = key + rest[key..].find('"')? + 1;
    let close = open + rest[open..].find('"')?;
    Some(rest[open..close].to_string())
}

fn config_of(home: &Path, profile: u8) -> PathBuf {
    home.join(format!(".claude-profile-{profile}/.claude.json"))
}

/// Which version of a file or directory was read.  A directory's mtime
/// moves when an entry is added, removed or renamed; a rewrite by rename
/// moves the inode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        ino: m.ino(),
        size: m.size(),
        mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
    })
}

/// [`discover_profiles`] and [`profile_email`] for the scan, which asks
/// every two seconds: `$HOME` is listed again only when its mtime moved,
/// and a `.claude.json` (about 300 KB each here) is read again only
/// when it changed.  Bounded by the profiles that exist.
#[derive(Default)]
pub(super) struct ProfileCache {
    home: Option<(PathBuf, Stamp, Vec<u8>)>,
    emails: HashMap<u8, (Stamp, Option<String>)>,
    pub(super) reads: u64,
}

impl ProfileCache {
    /// Every profile in `$HOME`, in order, with its account address.
    pub(super) fn read(&mut self) -> Vec<(u8, Option<String>)> {
        match std::env::var_os("HOME") {
            Some(home) => self.read_in(Path::new(&home)),
            None => Vec::new(),
        }
    }

    pub(super) fn read_in(&mut self, home: &Path) -> Vec<(u8, Option<String>)> {
        let Some(now) = stamp(home) else {
            self.home = None;
            self.emails.clear();
            return Vec::new();
        };
        let profiles = match &self.home {
            Some((path, was, profiles)) if path == home && *was == now => profiles.clone(),
            _ => {
                if self.home.as_ref().is_some_and(|(path, ..)| path != home) {
                    self.emails.clear();
                }
                self.reads += 1;
                let profiles = discover_profiles_in(home);
                self.home = Some((home.to_path_buf(), now, profiles.clone()));
                profiles
            }
        };
        self.emails.retain(|p, _| profiles.contains(p));
        profiles.into_iter().map(|p| (p, self.email(home, p))).collect()
    }

    fn email(&mut self, home: &Path, profile: u8) -> Option<String> {
        let Some(now) = stamp(&config_of(home, profile)) else {
            self.emails.remove(&profile);
            return None;
        };
        if let Some((was, email)) = self.emails.get(&profile)
            && *was == now
        {
            return email.clone();
        }
        self.reads += 1;
        let email = profile_email(home, profile);
        self.emails.insert(profile, (now, email.clone()));
        email
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(addr: &str) -> String {
        format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{addr}\"\n  }}\n}}\n")
    }

    #[test]
    fn an_unchanged_profile_is_not_read_again() {
        let home = std::env::temp_dir().join(format!("cc-profile-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for n in [1u8, 3] {
            std::fs::create_dir_all(home.join(format!(".claude-profile-{n}"))).unwrap();
        }
        std::fs::write(config_of(&home, 1), config("a@x")).unwrap();
        let mut cache = ProfileCache::default();
        let want = vec![(1u8, Some("a@x".to_string())), (3u8, None)];
        assert_eq!(cache.read_in(&home), want);
        assert_eq!(cache.reads, 2, "the home listing and one config");
        assert_eq!(cache.read_in(&home), want);
        assert_eq!(cache.reads, 2, "nothing changed, nothing read");

        std::fs::write(config_of(&home, 1), config("bb@x")).unwrap();
        assert_eq!(cache.read_in(&home)[0].1.as_deref(), Some("bb@x"));
        assert_eq!(cache.reads, 3, "a changed config is read again");

        std::fs::create_dir_all(home.join(".claude-profile-2")).unwrap();
        let got: Vec<u8> = cache.read_in(&home).into_iter().map(|(n, _)| n).collect();
        assert_eq!(got, vec![1, 2, 3], "a new profile shows up");
        assert_eq!(cache.reads, 4);
        std::fs::remove_dir_all(&home).unwrap();
        assert!(cache.read_in(&home).is_empty());
    }
}
