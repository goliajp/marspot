//! Which process answers for the app's sessions.
//!
//! macOS puts an app launch and everything it starts into one
//! coalition, for life, and LaunchServices holds one registered
//! application answerable for a coalition's members.  A coalition whose
//! application is gone belongs to the next program that registers from
//! inside it: a stranger started in a pane then has every session
//! listed as its own, and quitting it from the Dock signals them all.
//! So the app's shell stays for as long as a session does, and writes
//! here who it is.  A session reads it to know whose exit to watch for,
//! and what to start when that process dies without leaving one behind:
//! only a member of a coalition can start another member.
//!
//! One file in the state root, rewritten in place so that a watcher's
//! descriptor keeps seeing it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Set for a shell that is to come up with no window and no Dock icon,
/// only answering for the sessions.
pub const RESIDENT_ENV: &str = "MARSPOT_RESIDENT";

/// Its value when a session started that shell, after the one that
/// answered died.
pub const RESIDENT_STARTED: &str = "started";

/// Set, to the pid it takes over from, for a shell a session started
/// because a shell outside the sessions' coalition asked for one in it.
pub const HEIR_ENV: &str = "MARSPOT_HEIR_OF";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub pid: i32,
    /// Unknown when the system would not say.
    pub coalition: Option<u64>,
    /// What a session starts to get another shell.
    pub exe: PathBuf,
    /// Set by a shell that found the sessions in a coalition that is
    /// not its own: it asks for a shell to be started there and will
    /// leave when one answers.
    pub heir_in: Option<u64>,
    /// Whether that heir is to come up with no window.
    pub heir_resident: bool,
    /// What the shell that asks hands its heir, as environment.
    pub heir_env: Vec<(String, String)>,
}

pub fn path() -> PathBuf {
    crate::paths::state_root().join("owner")
}

/// Where an heir says it is waiting for the lock.
pub fn heir_path() -> PathBuf {
    crate::paths::state_root().join("owner.heir")
}

/// Sessions take turns here to start a shell, and its starts are
/// counted here.
pub fn spawn_lock_path() -> PathBuf {
    crate::paths::state_root().join("owner.spawn")
}

/// Where the app's bundle is, written by a shell that was started from
/// it and by an install.  A shell that runs an image outside the bundle
/// (an update) and a session both find the bundle through it.
pub fn bundle_file() -> PathBuf {
    crate::paths::state_root().join("bundle")
}

/// The app bundle `exe` is the main executable of, if it is in one.
pub fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.ends_with("MacOS") && contents.ends_with("Contents") && bundle.extension().is_some_and(|e| e == "app"))
        .then_some(bundle)
}

/// `exe` with every symbolic link on the way to it resolved.
/// LaunchServices matches a process to the earlier instances of its
/// app by the path it was started with, as text: an instance it
/// started itself has the resolved path, and a shell started as
/// `/tmp/…/Marspot.app/…` beside it was measured to be taken for
/// another application's process when the dead records of the app
/// were handed on.
pub fn resolved(exe: &Path) -> PathBuf {
    exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf())
}

/// The main executable of the bundle on file, when it is there.
pub fn installed_shell() -> Option<PathBuf> {
    let text = std::fs::read_to_string(bundle_file()).ok()?;
    let exe = resolved(&Path::new(text.trim()).join("Contents/MacOS/marspot-shell"));
    (bundle_of(&exe).is_some() && exe.is_file()).then_some(exe)
}

/// What to start for another shell, given what the record names.
/// LaunchServices takes a process for the app by the file it was
/// started from: a shell started from a copy outside the bundle is
/// listed under the copy's name, with a Dock tile of its own, and
/// opening the app starts a second one beside it.  So a recorded path
/// outside any bundle gives way to the bundle on file.
pub fn shell_to_start(recorded: &Path) -> PathBuf {
    if bundle_of(recorded).is_some() {
        return resolved(recorded);
    }
    installed_shell().unwrap_or_else(|| recorded.to_path_buf())
}

impl Record {
    pub fn encode(&self) -> String {
        let num = |v: Option<u64>| v.map_or_else(String::new, |c| c.to_string());
        let env: String = self.heir_env.iter().map(|(k, v)| format!("heir_env.{k}={v}\n")).collect();
        format!(
            "pid={}\ncoalition={}\nheir_in={}\nheir_resident={}\n{env}exe={}\n",
            self.pid,
            num(self.coalition),
            num(self.heir_in),
            u8::from(self.heir_resident),
            self.exe.display()
        )
    }

    /// `None` for text that is not a whole record: a reader can catch
    /// the file between its truncation and its write.
    pub fn parse(text: &str) -> Option<Record> {
        let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix('='));
        let exe = field("exe")?;
        if exe.is_empty() || !text.ends_with('\n') {
            return None;
        }
        Some(Record {
            pid: field("pid")?.parse().ok()?,
            coalition: field("coalition")?.parse().ok(),
            heir_in: field("heir_in")?.parse().ok(),
            heir_resident: field("heir_resident")? == "1",
            heir_env: text
                .lines()
                .filter_map(|l| l.strip_prefix("heir_env.")?.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            exe: exe.into(),
        })
    }

    pub fn read() -> Option<Record> {
        Record::parse(&std::fs::read_to_string(path()).ok()?)
    }

    pub fn write(&self) -> io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // one write: a reader sees nothing or all of it
        std::fs::File::create(path)?.write_all(self.encode().as_bytes())
    }

    /// Whether the process named is still there and still a shell: a
    /// pid outlives its process in a file.
    pub fn alive(&self) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        self.pid > 0
            && unsafe { libc::kill(self.pid, 0) } == 0
            && crate::platform::pid_exe_name(self.pid).as_deref() == Some("marspot-shell")
    }
}

/// The owner of the coalition `coalition`, for a log line: the shell on
/// record when it is alive and in that coalition.
pub fn owner_of(coalition: Option<u64>, record: Option<&Record>) -> Option<i32> {
    let r = record?;
    (coalition.is_some() && r.coalition == coalition && r.alive()).then_some(r.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            pid: 4242,
            coalition: Some(7239),
            exe: "/A B/Marspot.app/Contents/MacOS/marspot-shell".into(),
            heir_in: None,
            heir_resident: false,
            heir_env: Vec::new(),
        }
    }

    #[test]
    fn only_a_main_executable_names_its_bundle() {
        let p = Path::new;
        assert_eq!(bundle_of(p("/A B/Marspot.app/Contents/MacOS/marspot-shell")), Some(p("/A B/Marspot.app")));
        assert_eq!(bundle_of(p("/s/binaries/current/marspot-shell")), None);
        assert_eq!(bundle_of(p("/w/target/release/marspot-shell")), None);
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let mut r = sample();
        assert_eq!(Record::parse(&r.encode()), Some(r.clone()));
        r.heir_in = Some(1178);
        r.heir_resident = true;
        r.coalition = None;
        r.heir_env = vec![("MARSPOT_RESTORE_FRAME".into(), "10,20,800,600".into()), ("MARSPOT_UPDATE_WAITING".into(), "1".into())];
        assert_eq!(Record::parse(&r.encode()), Some(r));
        // as written by a shell from before an heir was handed anything
        let old = Record::parse("pid=1\ncoalition=2\nheir_in=2\nheir_resident=0\nexe=/x/marspot-shell\n").unwrap();
        assert_eq!(old.heir_env, vec![]);
    }

    #[test]
    fn a_file_caught_half_written_is_no_record() {
        let text = sample().encode();
        assert_eq!(Record::parse(""), None);
        assert_eq!(Record::parse(&text[..text.len() - 1]), None, "the last line is not finished");
        assert_eq!(Record::parse("pid=1\ncoalition=2\nheir_in=\nheir_resident=0\n"), None, "nothing to start");
    }

    #[test]
    fn only_a_live_shell_in_the_same_coalition_is_the_owner() {
        let dead = sample();
        assert_eq!(owner_of(Some(7239), Some(&dead)), None, "pid 4242 is no shell of ours");
        assert_eq!(owner_of(Some(7239), None), None);
        assert_eq!(owner_of(None, Some(&dead)), None);
    }
}
