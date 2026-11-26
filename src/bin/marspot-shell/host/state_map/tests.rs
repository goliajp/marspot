use std::path::{Path, PathBuf};

use super::*;

/// Every file under `root` that is not a directory, relative and
/// `/`-separated, links included and not followed.
fn files_under(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("a readable directory") {
            let p = e.expect("a readable entry").path();
            let meta = std::fs::symlink_metadata(&p).expect("an entry that is there");
            if meta.is_dir() {
                walk(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).expect("under the root").to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The bytes of a fact file that must stay as they are: all of it but a
/// trailing dropped line, whose count is raised in place.
fn settled(bytes: &[u8]) -> &[u8] {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let start = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let last = std::str::from_utf8(&bytes[start..]).unwrap_or("");
    if marspot_term::wire_files::fact_line::dropped_count(last).is_some() { &bytes[..start] } else { bytes }
}

/// What a run left that the table does not know, and the fact files
/// whose earlier bytes changed.
fn audit(before: &Path, after: &Path) -> (Vec<String>, Vec<String>) {
    let listed = files_under(after);
    let unknown = listed.iter().filter(|f| lookup(f).is_none()).cloned().collect();
    let mut rewritten = Vec::new();
    for f in files_under(before) {
        let Some(e) = lookup(&f) else { continue };
        let rotated_copy = ROTATED.iter().any(|(_, to)| to.contains(&e.path));
        if e.kind != Kind::Fact || NOT_APPEND_ONLY.contains(&e.path) || rotated_copy {
            continue;
        }
        // gone is allowed: a pruned day
        let Ok(now) = std::fs::read(after.join(&f)) else { continue };
        let then = std::fs::read(before.join(&f)).expect("the copy taken before");
        let kept = settled(&then);
        if !now.starts_with(kept) && !moved_on(e.path, kept, after, &listed) {
            rewritten.push(format!("{f} ({} bytes then, {} now)", then.len(), now.len()));
        }
    }
    (unknown, rewritten)
}

/// Whether `kept`, the bytes a file of the fact row `row` held, now
/// begin one of the files that row rotates to.
fn moved_on(row: &str, kept: &[u8], after: &Path, listed: &[String]) -> bool {
    let Some((_, to)) = ROTATED.iter().find(|(r, _)| *r == row) else { return false };
    listed.iter().filter(|f| lookup(f).is_some_and(|e| to.contains(&e.path))).any(|f| contents(&after.join(f)).starts_with(kept))
}

/// A file's bytes, a `.gz` one decompressed.
fn contents(path: &Path) -> Vec<u8> {
    if path.extension().is_none_or(|x| x != "gz") {
        return std::fs::read(path).expect("a listed file");
    }
    let out = std::process::Command::new("/usr/bin/gzip").arg("-dc").arg(path).output().expect("gzip runs");
    assert!(out.status.success(), "{} does not decompress", path.display());
    out.stdout
}

fn env_dir(var: &str) -> PathBuf {
    let dir = std::env::var_os(var).unwrap_or_else(|| panic!("{var} names no directory: run this through bin/test-state-map.sh"));
    let dir = PathBuf::from(dir);
    assert!(dir.is_dir(), "{} is not a directory", dir.display());
    dir
}

/// After a sandbox run of the app (`bin/test-state-map.sh`): every file
/// in its state root is in the table, and every fact file the run had
/// written to halfway was only appended to afterwards.
#[test]
#[ignore = "reads the state root a sandbox run left; bin/test-state-map.sh runs it"]
fn state_map_covers_every_file() {
    let (before, after) = (env_dir("MARSPOT_TEST_STATE_MAP_BEFORE"), env_dir("MARSPOT_TEST_STATE_MAP_ROOT"));
    let listed = files_under(&after);
    assert!(listed.iter().any(|f| f.starts_with("sessions/")), "the run left no session: it did not run the app");
    for f in &listed {
        let e = lookup(f).map_or("UNREGISTERED".into(), |e| format!("{:?} {}", e.kind, e.path));
        println!("{f}\t{e}");
    }
    let (unknown, rewritten) = audit(&before, &after);
    assert!(unknown.is_empty(), "files the state map does not know:\n  {}", unknown.join("\n  "));
    assert!(rewritten.is_empty(), "fact files whose earlier bytes changed:\n  {}", rewritten.join("\n  "));
    println!("{} files, all registered; fact files only appended to", listed.len());
}

#[test]
fn the_audit_finds_an_unknown_file_and_a_rewritten_fact() {
    let tmp = marspot_term::test_env::TempDir::new("state-map-audit");
    let (before, after) = (tmp.join("before"), tmp.join("after"));
    for d in [&before, &after] {
        std::fs::create_dir_all(d.join("usage/usage_samples")).unwrap();
        std::fs::create_dir_all(d.join("zombie")).unwrap();
    }
    std::fs::write(before.join("usage/usage_samples/20261126.tsv"), "a\nb\n").unwrap();
    std::fs::write(after.join("usage/usage_samples/20261126.tsv"), "a\nB\nc\n").unwrap();
    std::fs::write(before.join("zombie/told.tsv"), "1\n").unwrap();
    std::fs::write(after.join("zombie/told.tsv"), "2\n").unwrap();
    std::fs::write(after.join("surprise.bin"), "").unwrap();
    let (unknown, rewritten) = audit(&before, &after);
    assert_eq!(unknown, ["surprise.bin"]);
    assert_eq!(rewritten, ["usage/usage_samples/20261126.tsv (4 bytes then, 6 now)"], "a rewritten derived file is no finding");
    std::fs::write(after.join("usage/usage_samples/20261126.tsv"), "a\nb\nc\n").unwrap();
    assert!(audit(&before, &after).1.is_empty());
}

#[test]
fn the_audit_lets_a_dropped_count_rise_and_a_trimmed_draft_change() {
    let tmp = marspot_term::test_env::TempDir::new("state-map-audit-allowed");
    let (before, after) = (tmp.join("before"), tmp.join("after"));
    let day = "usage/usage_samples/20261126.tsv";
    for d in [&before, &after] {
        std::fs::create_dir_all(d.join("usage/usage_samples")).unwrap();
    }
    let fact = "1764115200\tk\tv=1\n";
    std::fs::write(before.join(day), format!("{fact}1764115201\tdropped\tdropped=00000000000000000001\n")).unwrap();
    std::fs::write(after.join(day), format!("{fact}1764115201\tdropped\tdropped=00000000000000000005\n")).unwrap();
    std::fs::write(before.join("shell_launches.tsv"), "1\n2\n").unwrap();
    std::fs::write(after.join("shell_launches.tsv"), "2\n3\n").unwrap();
    assert_eq!(audit(&before, &after), (vec![], vec![]));
    std::fs::write(after.join(day), "1764115200\tk\tv=2\n").unwrap();
    assert_eq!(audit(&before, &after).1.len(), 1, "the facts before the count still may not change");
}

#[test]
fn the_audit_lets_a_log_restart_only_when_its_bytes_moved_to_a_rotated_file() {
    let tmp = marspot_term::test_env::TempDir::new("state-map-audit-rotated");
    let (before, after) = (tmp.join("before"), tmp.join("after"));
    for d in [&before, &after] {
        std::fs::create_dir_all(d.join("logs")).unwrap();
    }
    std::fs::write(before.join("control-audit.log"), "a\nb\n").unwrap();
    std::fs::write(after.join("control-audit.log"), "b\n").unwrap();
    assert_eq!(audit(&before, &after).1, ["control-audit.log (4 bytes then, 2 now)"], "cut in place, moved nowhere");
    std::fs::write(after.join("control-audit.log"), "snapshot\n").unwrap();
    std::fs::write(after.join("control-audit.log.1"), "a\nb\nc\n").unwrap();
    std::fs::write(before.join("control-audit.log.1"), "older\n").unwrap();
    assert_eq!(audit(&before, &after), (vec![], vec![]), "rotated: the old bytes begin .1, and .1 itself was renamed over");

    std::fs::write(before.join("logs/marspot.log"), "x\ny\n").unwrap();
    std::fs::write(after.join("logs/marspot.log"), "z\n").unwrap();
    assert_eq!(audit(&before, &after).1.len(), 1);
    let gz = after.join("logs/marspot.00000000000000000002.log.gz");
    let mut zip = std::process::Command::new("/usr/bin/gzip")
        .arg("-c")
        .stdin(std::process::Stdio::piped())
        .stdout(std::fs::File::create(&gz).unwrap())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut zip.stdin.take().unwrap(), b"x\ny\nq\n").unwrap();
    assert!(zip.wait().unwrap().success());
    assert_eq!(audit(&before, &after), (vec![], vec![]), "the old bytes begin the compressed rotated log");
}

#[test]
fn every_pattern_is_well_formed_and_listed_once() {
    for (i, e) in STATE_MAP.iter().enumerate() {
        assert!(!e.path.starts_with('/') && !e.path.ends_with('/'), "{}", e.path);
        let mut rest = e.path;
        while let Some(at) = rest.find('<') {
            let end = rest[at..].find('>').unwrap_or_else(|| panic!("unclosed placeholder in {}", e.path)) + at;
            assert!(["id", "n", "date", "uuid", "sid"].contains(&&rest[at + 1..end]), "unknown placeholder in {}", e.path);
            rest = &rest[end + 1..];
        }
        assert!(!STATE_MAP[..i].iter().any(|o| o.path == e.path), "{} is listed twice", e.path);
        assert!(!e.writer.is_empty() && !e.cap.is_empty(), "{}", e.path);
    }
    for p in NOT_APPEND_ONLY {
        assert!(STATE_MAP.iter().any(|e| e.path == *p && e.kind == Kind::Fact), "{p} is no fact row");
    }
    for p in ROTATED.iter().flat_map(|(from, to)| std::iter::once(from).chain(to.iter())) {
        assert!(STATE_MAP.iter().any(|e| e.path == *p && e.kind == Kind::Fact), "{p} is no fact row");
    }
}

#[test]
fn files_of_today_find_their_rows() {
    let cases = [
        ("sessions/41/entry.toml", Kind::Draft, true),
        ("sessions/41/bytelog.seg.3", Kind::Draft, true),
        ("sessions/41/state.bin.tmp", Kind::Runtime, false),
        ("plugins/claudecode/alloc_events/20261126.tsv", Kind::Fact, true),
        ("plugins/claudecode/model/2fec8734-bf76-478b-8044-20261126b7e4", Kind::Derived, true),
        ("plugins/claudecode/model/.2fec8734-bf76-478b-8044-20261126b7e4.tmp", Kind::Runtime, false),
        ("plugins/claudecode/model/session-7f", Kind::Derived, true),
        ("sessions/41/scrollback.cold.bin", Kind::Derived, true),
        ("sessions/41/scrollback.cold.bin.clustertext", Kind::Derived, true),
        ("sessions/41/scrollback.bin.marks", Kind::Derived, true),
        ("sessions/41/scrollback.idx.corrupt-1764115200", Kind::Draft, false),
        ("sessions/41/sock", Kind::Runtime, true),
        ("control.sock", Kind::Runtime, true),
        ("logs/marspot.log.rotate-lock", Kind::Runtime, true),
        ("download/marspot.tar.d/marspot-shell", Kind::Runtime, false),
        ("usage/usage_samples/20261126.tsv", Kind::Fact, true),
        ("peerbus/kimi/session_abc.jsonl", Kind::Fact, true),
        ("peerbus/3-in.sock", Kind::Runtime, true),
        ("helpers/7/home/.claude.json", Kind::External, false),
        ("zombie/orphans.tsv", Kind::Derived, true),
        ("handoff/483.md", Kind::Draft, true),
        ("settings.toml", Kind::UserConfig, true),
        ("logs/marspot.00000000001764115200.log.gz", Kind::Fact, true),
        ("test-shell-core.run.log", Kind::Runtime, false),
    ];
    for (path, kind, across) in cases {
        let e = lookup(path).unwrap_or_else(|| panic!("{path} has no row"));
        assert_eq!((e.kind, e.across_images), (kind, across), "{path} matched {}", e.path);
    }
    assert!(lookup("zombie/unheard-of.tsv").is_none());
    assert!(lookup("sessions/x/entry.toml").is_none());
}

#[test]
fn directories_are_known_by_the_files_under_them() {
    for dir in ["plugins", "plugins/claudecode", "plugins/kimi", "zombie", "helpers", "peerbus/gemini", "sessions", "usage"] {
        assert!(has_dir(dir), "{dir}");
    }
    assert!(!has_dir("nobody"));
    assert!(!has_dir("settings.toml"));
    assert!(is_registered("zombie/told.tsv") && is_registered("plugins/claudecode/model"));
    assert!(!is_registered("anything.tmp"), "the catch-all rows vouch for no path the source names");
}
