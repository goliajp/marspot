//! Registering the status-line hook with Claude Code.
//!
//! Claude Code learns about the hook from one line in its own
//! `settings.json`.  There is no other channel: no environment
//! variable, and the `--settings` flag covers a single launch, while
//! most sessions are started by the user's own alias.
//!
//! Which makes this the one place marspot writes into another
//! program's configuration, so the rules are strict:
//!
//!   * it happens only while `claudecode.statusline_hook` is on, which
//!     is off by default and is a switch the user flips;
//!   * a status line the user already wrote is never taken away — it
//!     is chained (see `run_chained_statusline`) and put back on the
//!     way out;
//!   * turning the switch off restores the file, and the surrounding
//!     text survives the round trip byte for byte;
//!   * nothing is written that does not parse as JSON afterwards.
//!
//! Reconciliation is continuous rather than a one-off install step:
//! the desired state is a setting, so the answer to "what if the user
//! edits settings.json by hand" and "what if the binary moved" is the
//! same answer, and neither needs a script that a shipped marspot
//! would not have.

use std::fs;
use std::path::PathBuf;

/// The shell binary version that first understood `--cc-statusline`.
///
/// Anything older falls through its CLI to the GUI start and comes up
/// as a full supervisor — with claude calling it on every render.
/// Checked rather than assumed.
pub(super) const MIN_HOOK_SHELL: (u32, u32, u32) = (0, 7, 116);

/// Every Claude Code settings file on this machine, deduplicated.
///
/// The profile directories commonly symlink one shared file;
/// canonicalising means it is read and written once rather than once
/// per profile.
pub(crate) fn cc_settings_files() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let mut dirs = vec![home.join(".claude")];
    if let Ok(rd) = fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".claude-profile-") {
                dirs.push(e.path());
            }
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let f = d.join("settings.json");
        // A config dir with no settings file yet is how every fresh
        // install of Claude Code looks; it is named all the same, so
        // that what belongs in the file can be put there.
        let real = match f.canonicalize() {
            Ok(real) => real,
            Err(_) => match d.canonicalize() {
                Ok(dir) => dir.join("settings.json"),
                Err(_) => continue,
            },
        };
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

/// The binary the hook should name: the newest one that knows the
/// flag, bundle first.
///
/// The bundle path is stable and a cold launch refreshes it; while an
/// older bundle is still pinned open by the running app,
/// `binaries/current/` holds the newer shell — which is the very
/// binary the bundle would exec into anyway.
pub(super) fn hook_binary() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent() {
            cands.push(dir.join("marspot-shell"));
        }
    cands.push(
        marspot_term::paths::state_root()
            .join("binaries")
            .join("current")
            .join("marspot-shell"),
    );
    cands.into_iter().find(|c| shell_at_least(c, MIN_HOOK_SHELL))
}

pub(super) fn shell_at_least(bin: &std::path::Path, min: (u32, u32, u32)) -> bool {
    let Ok(out) = std::process::Command::new(bin)
        .arg("--version")
        .env("MARSPOT_NO_REDIRECT", "1")
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(v) = text
        .split_whitespace()
        .nth(1)
        .map(|v| v.trim_end_matches(|c: char| !c.is_ascii_digit()))
    else {
        return false;
    };
    let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let got = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    got >= min
}

/// Single-quote for `sh -c`, which is how claude runs the command.
///
/// Needed because the state root's path contains a space
/// ("Application Support"); unquoted, claude never invokes it at all.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(super) fn hook_command(bin: &std::path::Path, chained: &str) -> String {
    let base = format!("{} --cc-statusline", sh_quote(&bin.to_string_lossy()));
    if chained.is_empty() {
        base
    } else {
        format!("{base} --chain {}", sh_quote(chained))
    }
}

/// The command our hook was told to run after itself, if any.
pub(super) fn chained_out_of(cmd: &str) -> String {
    let words = sh_split(cmd);
    match words.iter().position(|w| w == "--chain") {
        Some(i) => words.get(i + 1).cloned().unwrap_or_default(),
        None => String::new(),
    }
}

/// Enough of a shell word split to read back what `sh_quote` wrote.
pub(super) fn sh_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The `statusLine` command in a settings file, if there is one.
///
/// A scan rather than a JSON parse: the file is the user's, it is
/// rewritten by cutting text so that their formatting survives, and
/// the same scan is what tells the cut where to start.
#[cfg(test)]
pub(super) fn status_line_command(text: &str) -> Option<(usize, String)> {
    status_line_span(text).map(|(key, _, cmd)| (key, cmd))
}

/// `status_line_command`, plus where the command's quoted value sits,
/// so an edit replaces those bytes however the writer escaped them
/// (some JSON writers turn every `/` into `\/`).
fn status_line_span(text: &str) -> Option<(usize, std::ops::Range<usize>, String)> {
    let key = text.find("\"statusLine\"")?;
    let cmd_key = text[key..].find("\"command\"")? + key;
    let colon = text[cmd_key..].find(':')? + cmd_key;
    let open = text[colon..].find('"')? + colon + 1;
    let mut end = open;
    let bytes = text.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'"' => break,
            _ => end += 1,
        }
    }
    let cmd = json_unescape(text.get(open..end)?)?;
    Some((key, open - 1..end + 1, cmd))
}

/// The value of a JSON string body (what sits between the quotes).
fn json_unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                let hi = u32::from_str_radix(&hex, 16).ok()?;
                let code = if (0xD800..0xDC00).contains(&hi) {
                    let (Some('\\'), Some('u')) = (chars.next(), chars.next()) else { return None };
                    let lo_hex: String = chars.by_ref().take(4).collect();
                    let lo = u32::from_str_radix(&lo_hex, 16).ok()?;
                    0x10000 + ((hi - 0xD800) << 10) + lo.checked_sub(0xDC00)?
                } else {
                    hi
                };
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

pub(super) fn json_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Bring every Claude Code settings file in line with the switch.
///
/// Cheap when there is nothing to do — a canonicalize and a read of a
/// small file per config dir — and it does nothing at all when the
/// switch is off and no hook of ours is present, which is the state
/// every machine starts in.
///
/// Returns the lines worth logging; the caller decides where they go.
pub(super) fn reconcile_statusline_hook(want: bool) -> Vec<String> {
    // A sandbox shares the developer's HOME, so these are the
    // developer's own settings files — and the installed app is
    // reconciling them at the same time.  Two marspots arguing over one
    // config removed the hook the real one had just installed, once a
    // minute, for as long as a test ran (2026-09-28).  The sandbox
    // stays out.
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return vec![
            "sandbox: leaving the status-line hook alone (not this marspot's settings)".into(),
        ];
    }
    reconcile_statusline_in(want, hook_binary().as_deref(), &cc_settings_files())
}

/// The reconciliation itself, with the two things it reads off the
/// machine — which binary to name, and which files to edit — handed
/// in, so it can be exercised against a settings file that is not the
/// user's.
pub(super) fn reconcile_statusline_in(
    want: bool,
    bin: Option<&std::path::Path>,
    files: &[PathBuf],
) -> Vec<String> {
    let mut notes = Vec::new();
    for path in files {
        let path = path.as_path();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            // no file yet: there is one to write only when the hook is wanted
            Err(e) if want && e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => continue,
        };
        let span = status_line_span(&text);
        let found = span.as_ref().map(|(at, _, cmd)| (*at, cmd.clone()));
        let at_value = span.map(|(_, r, _)| r);
        let swap = |cmd: &str| {
            let r = at_value.clone().expect("found implies a span");
            format!("{}{}{}", &text[..r.start], json_quote(cmd), &text[r.end..])
        };
        let ours = found
            .as_ref()
            .is_some_and(|(_, c)| c.contains("--cc-statusline"));
        let new_text = match (want, &found, ours) {
            // Wanted and already ours: keep it naming a binary that
            // knows the flag.  The bundle's copy is replaced on a cold
            // launch, so this is how a hook installed against
            // `binaries/current/` moves back to the stable path.
            (true, Some((_, cmd)), true) => {
                let Some(bin) = bin else { continue };
                let want_cmd = hook_command(bin, &chained_out_of(cmd));
                if *cmd == want_cmd {
                    continue;
                }
                notes.push(format!("{}: repointed", path.display()));
                swap(&want_cmd)
            }
            // Wanted, and somebody else's status line is in the slot.
            // Claude Code allows exactly one, so take the slot and run
            // theirs from inside ours.
            (true, Some((_, cmd)), false) => {
                let Some(bin) = bin else { continue };
                notes.push(format!("{}: installed, chaining {cmd}", path.display()));
                swap(&hook_command(bin, cmd))
            }
            // Wanted, nothing in the slot.
            (true, None, _) => {
                let Some(bin) = bin else {
                    notes.push(format!(
                        "{}: no marspot-shell new enough for the hook",
                        path.display()
                    ));
                    continue;
                };
                // the member as Claude Code itself would write it, so a
                // file in its form stays in its form
                let member = format!(
                    "\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": {}\n  }}",
                    json_quote(&hook_command(bin, ""))
                );
                notes.push(format!("{}: installed", path.display()));
                let bare: String = text.split_whitespace().collect();
                match text.find('{') {
                    // nothing in the file, or nothing in the object
                    _ if bare.is_empty() || bare == "{}" => format!("{{{member}\n}}\n"),
                    Some(i) => format!("{}{member},{}", &text[..=i], &text[i + 1..]),
                    None => continue,
                }
            }
            // Not wanted, and ours is there: give the slot back.
            (false, Some((at, cmd)), true) => {
                let chained = chained_out_of(cmd);
                notes.push(format!("{}: removed", path.display()));
                if chained.is_empty() {
                    remove_status_line(&text, *at, at_value.clone().expect("found implies a span"))
                } else {
                    swap(&chained)
                }
            }
            // Not wanted and not ours — nothing of ours to undo.
            (false, _, _) => continue,
        };
        // A settings.json that will not parse would lock the user out
        // of their own tool, so the edit has to prove itself first.
        if !json_parses(&new_text) {
            notes.push(format!("{}: edit refused — would not parse", path.display()));
            continue;
        }
        if let Err(e) = write_through_symlink(path, &new_text) {
            notes.push(format!("{}: {e}", path.display()));
        }
    }
    notes
}

/// `text` minus its `statusLine` member, the rest verbatim.
///
/// Cutting text rather than re-serialising: this is a file people
/// hand-edit, and a round trip through a JSON writer would reflow
/// every line of it to remove one key.
pub(super) fn remove_status_line(text: &str, at: usize, value: std::ops::Range<usize>) -> String {
    let Some(rel) = text[at..].find('{') else {
        return text.to_string();
    };
    let mut j = at + rel;
    let b = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    while j < b.len() {
        let c = b[j];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    debug_assert!(value.start > at && value.end <= j);
    // A member only comes out together with one of its commas.
    let mut start = at;
    let mut k = j;
    while b.get(k).is_some_and(|c| *c == b' ' || *c == b'\t') {
        k += 1;
    }
    if b.get(k) == Some(&b',') {
        k += 1;
    } else {
        // Last member — the comma joining it sits in front.
        let mut pre = start;
        while pre > 0 && b[pre - 1].is_ascii_whitespace() {
            pre -= 1;
        }
        if pre > 0 && b[pre - 1] == b',' {
            start = pre - 1;
        }
    }
    // Take the whole line when nothing else shares it, and one of the
    // two newlines bracketing it — whichever is there — so a file
    // written on a single line goes back to being one.
    let bol = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if text[bol..start].trim().is_empty() {
        start = bol;
        if b.get(k) == Some(&b'\n') {
            k += 1;
        } else if start > 0 && b[start - 1] == b'\n' {
            start -= 1;
        }
    }
    format!("{}{}", &text[..start], &text[k..])
}

/// Structural check: braces, brackets and strings balance and the
/// text ends where they close.
///
/// Not a parser — the edits above only ever add or remove one whole
/// member, so what has to be caught is a stray comma or an unbalanced
/// brace, and that is what this catches.
pub(super) fn json_parses(text: &str) -> bool {
    let mut stack: Vec<u8> = Vec::new();
    let (mut in_str, mut esc, mut prev) = (false, false, 0u8);
    for &c in text.as_bytes() {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => stack.push(c),
            b'}' | b']' => {
                let want = if c == b'}' { b'{' } else { b'[' };
                if stack.pop() != Some(want) || prev == b',' {
                    return false;
                }
            }
            b',' if prev == b',' => return false,
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            prev = c;
        }
    }
    stack.is_empty() && !in_str
}

/// Replace the file's contents, keeping it the same file.
///
/// The profiles' `settings.json` are symlinks to one shared file;
/// renaming onto the link path would replace the link itself, so the
/// caller passes the canonical path and the temp file is made beside
/// it.
pub(crate) fn write_through_symlink(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(format!(".marspot-settings-{}.tmp", std::process::id()));
    fs::write(&tmp, text)?;
    if let Ok(md) = fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(md.permissions().mode()));
    }
    fs::rename(&tmp, path)
}
