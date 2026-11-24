//! The directories whose panes may not control other panes, one per
//! line in `<state dir>/control-deny.tsv`.  A directory listed denies
//! the panes in it and below it.  The shell refuses by it; the helper
//! panel shows it and has the shell change it; a person may edit it.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Tries at changing a file somebody else keeps changing.
const TRIES: usize = 5;
/// Symlinks followed from the list to its file at most.
const LINKS_MAX: usize = 16;

pub fn path() -> PathBuf {
    marspot_term::paths::state_root().join("control-deny.tsv")
}

/// The file a list at `path` is in: where its symlinks lead, though the
/// file there may not exist yet.
pub fn file_of(path: &Path) -> PathBuf {
    let mut at = path.to_path_buf();
    for _ in 0..LINKS_MAX {
        let Ok(to) = std::fs::read_link(&at) else { break };
        // an absolute target replaces the whole path
        at = at.parent().map_or_else(|| to.clone(), |dir| dir.join(&to));
    }
    at
}

fn norm(dir: &str) -> &str {
    if dir.len() > 1 { dir.trim_end_matches('/') } else { dir }
}

/// The directory a line of the file names; None for a comment or a
/// blank line.  White space around a name a person wrote is not the name's.
fn listed(line: &str) -> Option<&str> {
    Some(norm(line.trim())).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// `path` is `dir` or below it, component by component.
pub fn within(path: &str, dir: &str) -> bool {
    match path.strip_prefix(dir) {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || dir.ends_with('/'),
        None => false,
    }
}

/// What the list says of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cover {
    Free,
    /// Denied by a line of its own and no other.
    Own,
    /// Denied by this line, which names a directory above it.
    Above(String),
}

fn cover<'a>(dirs: impl Iterator<Item = &'a str>, cwd: &str) -> Cover {
    let mut own = false;
    let mut above: Option<&str> = None;
    for d in dirs.filter(|d| within(cwd, d)) {
        if d == cwd {
            own = true;
        } else if above.is_none_or(|a| d.len() < a.len()) {
            above = Some(d);
        }
    }
    match above {
        Some(d) => Cover::Above(d.to_string()),
        None if own => Cover::Own,
        None => Cover::Free,
    }
}

/// Which content a file had: another one has another stamp.
type Stamp = Option<(SystemTime, u64, u64)>;

fn stamp(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.ino(), m.len()))
}

/// The list, read again when its file is another or was modified.
pub struct DenyList {
    path: PathBuf,
    stamp: Stamp,
    dirs: HashSet<String>,
}

impl DenyList {
    pub fn at(path: PathBuf) -> Self {
        Self { path, stamp: None, dirs: HashSet::new() }
    }

    fn refresh(&mut self) {
        let now = stamp(&self.path);
        if now != self.stamp {
            self.stamp = now;
            let body = std::fs::read_to_string(&self.path).unwrap_or_default();
            self.dirs = body.lines().filter_map(listed).map(String::from).collect();
        }
    }

    /// Whether panes in `cwd` are refused, after a `stat` of the file.
    pub fn denies(&mut self, cwd: &str) -> bool {
        self.refresh();
        let cwd = norm(cwd);
        self.dirs.iter().any(|d| within(cwd, d))
    }

    /// Why panes in `cwd` are refused, if they are, after a `stat` of
    /// the file.
    pub fn cover(&mut self, cwd: &str) -> Cover {
        self.refresh();
        cover(self.dirs.iter().map(String::as_str), norm(cwd))
    }
}

/// What [`set`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Denied,
    Allowed,
    /// It was as wanted before.
    Unchanged,
    /// Not allowed: this line, of a directory above it, denies it, and
    /// is not this directory's to drop.
    CoveredBy(String),
}

/// Deny `cwd`, or drop the line that names it; None: whichever it is
/// not now.  Other lines and comments stay as they are.
pub fn set(path: &Path, cwd: &str, deny: Option<bool>) -> io::Result<Outcome> {
    set_before(path, cwd, deny, || {})
}

/// [`set`], with `ready` called each time the new content is about to
/// take the old one's place.
fn set_before(path: &Path, cwd: &str, deny: Option<bool>, mut ready: impl FnMut()) -> io::Result<Outcome> {
    if !cwd.starts_with('/') || cwd.trim() != cwd || cwd.chars().any(char::is_control) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{cwd:?} cannot be a line of the list")));
    }
    let cwd = norm(cwd);
    let file = file_of(path);
    for _ in 0..TRIES {
        let was = stamp(&file);
        let body = match std::fs::read_to_string(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let out = match (cover(body.lines().filter_map(listed), cwd), deny) {
            (Cover::Free, Some(false)) | (Cover::Own | Cover::Above(_), Some(true)) => return Ok(Outcome::Unchanged),
            (Cover::Above(line), _) => return Ok(Outcome::CoveredBy(line)),
            (Cover::Free, _) => Outcome::Denied,
            (Cover::Own, _) => Outcome::Allowed,
        };
        if replace(&file, &changed(&body, cwd, &out), was, &mut ready)? {
            return Ok(out);
        }
    }
    Err(io::Error::other("the list keeps changing"))
}

/// Put `body` in `file`'s place, with `file`'s permissions, unless it
/// is no longer the file stamped `was`.  The new file is left nowhere
/// but there.
fn replace(file: &Path, body: &str, was: Stamp, ready: &mut impl FnMut()) -> io::Result<bool> {
    let tmp = file.with_extension("tsv.tmp");
    let put = (|| {
        std::fs::write(&tmp, body)?;
        if let Ok(m) = std::fs::metadata(file) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        ready();
        // what a person wrote meanwhile is read and changed instead
        if stamp(file) != was {
            return Ok(false);
        }
        std::fs::rename(&tmp, file).map(|()| true)
    })();
    if !matches!(put, Ok(true)) {
        // there may be none to remove
        let _ = std::fs::remove_file(&tmp);
    }
    put
}

/// `body` with `cwd`'s own line dropped, or added.
fn changed(body: &str, cwd: &str, to: &Outcome) -> String {
    if *to == Outcome::Allowed {
        return body.split_inclusive('\n').filter(|l| listed(l) != Some(cwd)).collect();
    }
    let end = if body.is_empty() || body.ends_with('\n') { "" } else { "\n" };
    format!("{body}{end}{cwd}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, body: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("control-deny-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control-deny.tsv");
        if let Some(b) = body {
            std::fs::write(&path, b).unwrap();
        }
        path
    }

    fn done(p: &Path) {
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn helper_panel_deny_adds_and_allow_removes_one_directory() {
        let p = file("one", None);
        assert_eq!(set(&p, "/w/spg/", Some(true)).unwrap(), Outcome::Denied, "no file yet");
        let mut list = DenyList::at(p.clone());
        assert!(list.denies("/w/spg") && list.denies("/w/spg/sub") && !list.denies("/w/spg2"));
        assert_eq!(list.cover("/w/spg"), Cover::Own);
        assert_eq!(list.cover("/w/spg/sub"), Cover::Above("/w/spg".into()));
        assert_eq!(set(&p, "/w/spg", Some(true)).unwrap(), Outcome::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "/w/spg\n", "asked twice, listed once");
        assert_eq!(set(&p, "/w/spg", Some(false)).unwrap(), Outcome::Allowed);
        assert_eq!(set(&p, "/w/spg", Some(false)).unwrap(), Outcome::Unchanged);
        assert_eq!(DenyList::at(p.clone()).cover("/w/spg"), Cover::Free);
        // nothing said: whichever it is not
        assert_eq!(set(&p, "/w/spg", None).unwrap(), Outcome::Denied);
        assert_eq!(set(&p, "/w/spg", None).unwrap(), Outcome::Allowed);
        done(&p);
    }

    #[test]
    fn helper_panel_allow_never_drops_a_line_of_another_directory() {
        let body = "# mine\n/w/other\n\n/w/spg\n/w\n";
        let p = file("above", Some(body));
        for deny in [Some(false), None] {
            assert_eq!(set(&p, "/w/spg", deny).unwrap(), Outcome::CoveredBy("/w".into()));
        }
        assert_eq!(set(&p, "/w/spg/x", Some(true)).unwrap(), Outcome::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
        std::fs::write(&p, "# mine\n/w/other\n\n/w/spg/\n/w/spg/sub").unwrap();
        assert_eq!(set(&p, "/w/spg", Some(false)).unwrap(), Outcome::Allowed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# mine\n/w/other\n\n/w/spg/sub", "nor one of a directory below");
        assert_eq!(set(&p, "/w/new", Some(true)).unwrap(), Outcome::Denied);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# mine\n/w/other\n\n/w/spg/sub\n/w/new\n");
        done(&p);
    }

    #[test]
    fn helper_panel_deny_refuses_names_that_are_no_line() {
        let p = file("names", None);
        for bad in ["/w/a\n/w", "/w/a\rb", "/w/a\tb", "/w/\u{7f}", "/w/a ", "", "w"] {
            assert_eq!(set(&p, bad, Some(true)).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        assert!(!p.exists());
        std::fs::write(&p, " /w/a \n").unwrap();
        assert!(DenyList::at(p.clone()).denies("/w/a"), "typed with spaces around it");
        assert_eq!(set(&p, "/w/a", Some(false)).unwrap(), Outcome::Allowed);
        done(&p);
    }

    #[test]
    fn helper_panel_deny_keeps_what_was_written_meanwhile() {
        let p = file("race", Some("/w/a\n"));
        let mut n = 0;
        let out = set_before(&p, "/w/b", Some(true), || {
            n += 1;
            if n == 1 {
                // an editor's save: another file in its place
                let other = p.with_extension("edit");
                std::fs::write(&other, "/w/a\n/w/c\n").unwrap();
                std::fs::rename(&other, &p).unwrap();
            }
        });
        assert_eq!((out.unwrap(), n), (Outcome::Denied, 2));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "/w/a\n/w/c\n/w/b\n");
        let mut n = 0;
        let out = set_before(&p, "/w/d", Some(true), || {
            n += 1;
            let body = std::fs::read_to_string(&p).unwrap();
            std::fs::write(&p, format!("{body}/w/x{n}\n")).unwrap();
        });
        assert!(out.is_err() && n == TRIES, "gives up");
        done(&p);
    }

    #[test]
    fn helper_panel_deny_keeps_the_files_mode_and_leaves_no_new_file_behind() {
        use std::os::unix::fs::PermissionsExt;
        let p = file("mode", Some("/w/a\n"));
        let tmp = p.with_extension("tsv.tmp");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(set(&p, "/w/b", Some(true)).unwrap(), Outcome::Denied);
        assert_eq!(mode(&p), 0o600);
        assert_eq!(set(&p, "/w/b", Some(false)).unwrap(), Outcome::Allowed);
        assert_eq!(mode(&p), 0o600);
        // given up on: a list that changes at every try
        let mut n = 0;
        let out = set_before(&p, "/w/c", Some(true), || {
            n += 1;
            std::fs::write(&p, format!("/w/a\n/w/x{n}\n")).unwrap();
        });
        assert!(out.is_err());
        assert!(!tmp.exists(), "nothing left beside the list");
        done(&p);
    }

    #[test]
    fn helper_panel_deny_changes_the_file_a_symlink_leads_to() {
        let p = file("link", None);
        let dir = p.parent().unwrap();
        std::fs::create_dir(dir.join("kept")).unwrap();
        std::os::unix::fs::symlink("kept/list", &p).unwrap();
        for (cwd, body) in [("/w/a", "/w/a\n"), ("/w/b", "/w/a\n/w/b\n")] {
            assert_eq!(set(&p, cwd, Some(true)).unwrap(), Outcome::Denied);
            assert!(std::fs::symlink_metadata(&p).unwrap().is_symlink());
            assert_eq!(std::fs::read_to_string(dir.join("kept/list")).unwrap(), body);
        }
        done(&p);
    }
}
