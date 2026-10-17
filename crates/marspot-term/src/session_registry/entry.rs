//! The `entry.toml` format: one `key = value` per line, strings quoted.

use std::io::{self, Write};
use std::path::PathBuf;

use super::{session_dir, session_entry_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: u64,
    pub pid: i32,
    pub socket: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub cwd: String,
    pub proto_version: u32,
    pub created_at_unix: u64,
    /// RFC-003 Amendment 7: shm region name the L3 publishes into.
    /// A freshly-spawned L2 after a silent-update swap can shm_open
    /// this name to reattach to a surviving L3's framebuffer (the fd
    /// doesn't survive the swap; the name does).  Empty string =
    /// legacy entry written before this field existed.
    pub shm_name: String,
    /// RFC-003 §6 cc: the L3's forkpty child PID (the zsh / login
    /// shell at the other end of the PTY).  L1-side plugins
    /// (claudecode, future ones) walk pidtree from this pid to find
    /// descendants like `claude` they want to badge / send input to.
    /// `0` = legacy entry written before this field existed.
    pub shell_child_pid: i32,
}

/// Quote a string value.  `\n` and `\r` are escaped so a value can
/// never split a line; a reader from before those escapes reads them
/// back as a literal backslash and letter.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Inverse of `quote`.  Old files only ever contain `\\` and `\"`, so
/// they read the same as before.  An unknown escape is kept verbatim.
fn unquote(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(e @ ('\\' | '"')) => out.push(e),
            Some(e) => {
                out.push('\\');
                out.push(e);
            }
            None => out.push('\\'),
        }
    }
    Some(out)
}

/// Write the entry atomically and durably: temp file, fsync, rename,
/// fsync of the directory.  On any failure the previous entry (or no
/// entry) is what stays on disk.
pub fn write_session_entry(entry: &SessionEntry) -> io::Result<()> {
    let dir = session_dir(entry.id);
    std::fs::create_dir_all(&dir)?;
    let path = session_entry_path(entry.id);
    let tmp = path.with_extension("toml.tmp");
    let mut s = String::new();
    use std::fmt::Write as _;
    writeln!(s, "# marspot session registry entry — RFC-003").ok();
    writeln!(s, "id = {}", entry.id).ok();
    writeln!(s, "pid = {}", entry.pid).ok();
    writeln!(s, "socket = {}", quote(&entry.socket.to_string_lossy())).ok();
    writeln!(s, "cols = {}", entry.cols).ok();
    writeln!(s, "rows = {}", entry.rows).ok();
    writeln!(s, "title = {}", quote(&entry.title)).ok();
    writeln!(s, "cwd = {}", quote(&entry.cwd)).ok();
    writeln!(s, "proto_version = {}", entry.proto_version).ok();
    writeln!(s, "created_at_unix = {}", entry.created_at_unix).ok();
    writeln!(s, "shm_name = {}", quote(&entry.shm_name)).ok();
    writeln!(s, "shell_child_pid = {}", entry.shell_child_pid).ok();
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(s.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    // the rename itself survives a crash only once the directory is synced;
    // failing that is not worth failing a write that already landed
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn invalid<T: Into<String>>(msg: T) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Parse one entry.toml. Tolerates unknown fields and blank/comment
/// lines; rejects only when a required field is missing or the value
/// can't be parsed at its declared type.
pub fn parse_session_entry_text(contents: &str) -> io::Result<SessionEntry> {
    use std::collections::HashMap;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        fields.insert(key, val);
    }
    let req_str = |k: &str| -> io::Result<String> {
        let raw = fields
            .get(k)
            .ok_or_else(|| invalid(format!("missing {k}")))?;
        unquote(raw).ok_or_else(|| invalid(format!("{k} not quoted")))
    };
    let req_num = |k: &str| -> io::Result<u64> {
        let raw = fields
            .get(k)
            .ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u64>()
            .map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_i32 = |k: &str| -> io::Result<i32> {
        let raw = fields
            .get(k)
            .ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<i32>()
            .map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_u16 = |k: &str| -> io::Result<u16> {
        let raw = fields
            .get(k)
            .ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u16>()
            .map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_u32 = |k: &str| -> io::Result<u32> {
        let raw = fields
            .get(k)
            .ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u32>()
            .map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    // Optional field for back-compat with entries written before
    // Amendment 7 added shm_name.  Missing / unquoted = "".
    let opt_str = |k: &str| -> String {
        fields
            .get(k)
            .and_then(|raw| unquote(raw))
            .unwrap_or_default()
    };
    let opt_i32 = |k: &str| -> i32 {
        fields
            .get(k)
            .and_then(|raw| raw.parse::<i32>().ok())
            .unwrap_or(0)
    };
    Ok(SessionEntry {
        id: req_num("id")?,
        pid: req_i32("pid")?,
        socket: PathBuf::from(req_str("socket")?),
        cols: req_u16("cols")?,
        rows: req_u16("rows")?,
        title: req_str("title")?,
        cwd: req_str("cwd")?,
        proto_version: req_u32("proto_version")?,
        created_at_unix: req_num("created_at_unix")?,
        shm_name: opt_str("shm_name"),
        shell_child_pid: opt_i32("shell_child_pid"),
    })
}

pub fn read_session_entry(id: u64) -> io::Result<SessionEntry> {
    let contents = std::fs::read_to_string(session_entry_path(id))?;
    parse_session_entry_text(&contents)
}
