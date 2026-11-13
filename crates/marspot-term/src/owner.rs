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
use std::path::PathBuf;

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

impl Record {
    pub fn encode(&self) -> String {
        let num = |v: Option<u64>| v.map_or_else(String::new, |c| c.to_string());
        format!(
            "pid={}\ncoalition={}\nheir_in={}\nheir_resident={}\nexe={}\n",
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
        Record { pid: 4242, coalition: Some(7239), exe: "/A B/Marspot.app/Contents/MacOS/marspot-shell".into(), heir_in: None, heir_resident: false }
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let mut r = sample();
        assert_eq!(Record::parse(&r.encode()), Some(r.clone()));
        r.heir_in = Some(1178);
        r.heir_resident = true;
        r.coalition = None;
        assert_eq!(Record::parse(&r.encode()), Some(r));
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
