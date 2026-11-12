//! A program started by launchd, in a coalition of its own.
//!
//! macOS puts an app launch and everything it starts into one
//! coalition, and a process never leaves the one it was born in.  Only
//! launchd starts a process in a new one.  So what is to have a
//! coalition of its own is handed to launchd as a job: a host
//! (`pane_host`) that starts the program, says which pid it is and
//! exits.  The job is then taken off launchd's list: the program stays,
//! in the coalition the job was given, a child of launchd, and nothing
//! of the job is left for the system to end when the app quits --
//! launchd stops the jobs an app asked for along with the app.
//!
//! Which executable the host is decides who the privacy system charges
//! for what the program runs; see `pane_host`.

/// Every label of one state directory starts with this: a sandbox's
/// jobs never share a label with the installed app's.
fn label_prefix(state_root: &[u8]) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for b in state_root {
        hash = (hash ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    format!("com.marspot.pane.{hash:08x}.")
}

/// `key` names the job within its state directory: a session's id.
// off macOS nothing starts a job: this is built for its tests only
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn label_for(state_root: &[u8], key: &str) -> String {
    format!("{}{key}", label_prefix(state_root))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist(label: &str, host: &str, spec: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // interactive: launchd otherwise holds a job to less than an app gets.
    // the process group is left alone when the host goes and when the
    // job is removed: that is what lets the program stay
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>{}</string><string>{}</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>ProcessType</key><string>Interactive</string>\n\
         <key>AbandonProcessGroup</key><true/>\n\
         </dict></plist>\n",
        esc(label),
        esc(host),
        crate::pane_host::ARG,
        esc(spec)
    )
}

/// A started program, and what starting it took.
#[derive(Debug)]
pub struct Started {
    pub pid: i32,
    pub label: String,
    /// launchd taking the job.
    pub load_ms: u128,
    /// the host starting the program and answering.
    pub answer_ms: u128,
}

#[cfg(target_os = "macos")]
pub use macos::{coalition_of, start};

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Started, label_for, plist};
    use crate::pane_host::{self, Spec};

    /// How long launchd and the host get to produce the program.
    const HOST_WAIT: Duration = Duration::from_secs(5);
    const LAUNCHCTL: &str = "/bin/launchctl";

    fn domain() -> String {
        // SAFETY: getuid cannot fail and touches nothing.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    /// Have launchd run `host` with `spec`, under the label `key`
    /// gives, and take the job off its list once the host has answered.
    /// Where the host is to answer is filled in here.
    pub fn start(key: &str, host: &Path, mut spec: Spec) -> io::Result<Started> {
        let root = crate::paths::state_root();
        let dir = root.join("pane-jobs");
        fs::create_dir_all(&dir)?;
        let label = label_for(root.as_os_str().as_bytes(), key);
        let (spec_path, reply_path, plist_path) =
            (dir.join(format!("{key}.spec")), dir.join(format!("{key}.reply")), dir.join(format!("{key}.plist")));

        let _ = fs::remove_file(&reply_path);
        let reply_c = CString::new(reply_path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: a valid C string; mkfifo only creates the node.
        if unsafe { libc::mkfifo(reply_c.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // both ends, before the host exists: its write never waits for
        // a reader, and our read never sees an end of file
        let mut reply = fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&reply_path)?;

        spec.reply = reply_path.as_os_str().as_bytes().to_vec();
        // the environment can hold secrets: the file is ours alone and
        // the host removes it as soon as it has read it
        fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&spec_path)?.write_all(&spec.encode())?;
        fs::write(&plist_path, plist(&label, &host.to_string_lossy(), &spec_path.to_string_lossy()))?;

        let begun = Instant::now();
        let out = Command::new(LAUNCHCTL).arg("bootstrap").arg(domain()).arg(&plist_path).stdin(Stdio::null()).output()?;
        let _ = fs::remove_file(&plist_path);
        let loaded = begun.elapsed();
        let mut answered = loaded;
        let pid = if out.status.success() {
            let pid = read_reply(&mut reply);
            answered = begun.elapsed();
            let _ = Command::new(LAUNCHCTL).arg("bootout").arg(format!("{}/{label}", domain())).stdin(Stdio::null()).output();
            pid
        } else {
            Err(io::Error::other(format!("launchctl bootstrap: {}", String::from_utf8_lossy(&out.stderr).trim())))
        };
        let _ = fs::remove_file(&reply_path);
        let pid = pid.inspect_err(|_| {
            let _ = fs::remove_file(&spec_path);
        })?;
        Ok(Started { pid, label, load_ms: loaded.as_millis(), answer_ms: (answered - loaded).as_millis() })
    }

    fn read_reply(reply: &mut fs::File) -> io::Result<i32> {
        let deadline = Instant::now() + HOST_WAIT;
        let mut line = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd { fd: reply.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd, on a descriptor that is open.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the pane host did not answer"));
            }
            let mut buf = [0u8; 256];
            match reply.read(&mut buf) {
                Ok(k) => line.extend_from_slice(&buf[..k]),
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(e) => return Err(e),
            }
            if line.contains(&b'\n') {
                return pane_host::parse_reply(&String::from_utf8_lossy(&line));
            }
        }
    }

    /// The jetsam coalition `pid` is in, 0 when the system will not say.
    /// Read for a log line only.
    pub fn coalition_of(pid: i32) -> u64 {
        // `PROC_PIDCOALITIONINFO` and its struct, from the system's
        // private half of <sys/proc_info.h>
        const FLAVOR: libc::c_int = 20;
        #[repr(C)]
        #[derive(Default)]
        struct Info {
            id: [u64; 2],
            reserved: [u64; 3],
        }
        let mut info = Info::default();
        let size = std::mem::size_of::<Info>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes; an unknown flavor fails
        // without writing.
        let n = unsafe { libc::proc_pidinfo(pid, FLAVOR, 0, (&raw mut info).cast(), size) };
        if n == size { info.id[1] } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_state_directories_never_share_a_label() {
        assert_eq!(label_for(b"/tmp/a", "7"), label_for(b"/tmp/a", "7"));
        assert_ne!(label_for(b"/tmp/a", "7"), label_for(b"/tmp/b", "7"));
        assert_ne!(label_for(b"/tmp/a", "7"), label_for(b"/tmp/a", "8"));
        assert!(label_for(b"/tmp/a", "7").starts_with("com.marspot.pane."));
    }

    #[test]
    fn a_path_with_markup_in_it_stays_one_string_in_the_plist() {
        let p = plist("l", "/a&b/<c>", "/s");
        assert!(p.contains("<string>/a&amp;b/&lt;c&gt;</string>"));
        assert!(p.contains("<string>--pane-host</string>"));
    }
}
