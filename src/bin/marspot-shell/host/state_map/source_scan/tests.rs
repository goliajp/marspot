use super::*;

fn scan(files: &[(&str, &str)]) -> Vec<String> {
    let srcs: Vec<(String, String)> = files.iter().map(|(f, t)| (f.to_string(), prepare(t))).collect();
    let mut got: Vec<String> = paths(&srcs).iter().map(Found::shown).collect();
    got.sort();
    got.dedup();
    got
}

#[test]
fn the_scan_reads_the_plain_shapes() {
    let got = scan(&[
        ("x/a.rs", "const F: &str = \"f.tsv\";\nfn p() { state_root().join(\"zombie\")\n    .join(F); }\nfn q() { default_state_root().join(\"no\"); }"),
        ("x/plugins/kimi/b.rs", "fn l(h: &dyn H) { let Ok(dir) = h.state_dir() else { return };\n let t = dir.join(REFUSED); }\nconst REFUSED: &str = \"refused.tsv\";"),
        ("x/c.rs", "fn r() { let root = paths::state_root();\n root.join(\"a\").join(name).join(\"b\"); session_dir(7).join(\"guard\"); }"),
        ("x/e.rs", "#[cfg(test)]\nmod tests;\nfn u() { state_root().join(\"after-a-test-file\"); }"),
    ]);
    assert_eq!(got, ["a", "after-a-test-file", "plugins/kimi/refused.tsv", "sessions/{}/guard", "zombie/f.tsv"]);
}

#[test]
fn code_after_an_inline_test_module_is_read_and_the_module_is_not() {
    let got = scan(&[(
        "x/d.rs",
        "fn s() { state_root().join(\"live\"); }\n#[cfg(test)]\nmod tests {\n fn t() { state_root().join(\"test-only\"); }\n fn u() { let s = \"}\"; }\n}\nfn v() { state_root().join(\"after\"); }",
    )]);
    assert_eq!(got, ["after", "live"]);
}

#[test]
fn a_helper_that_returns_a_path_is_a_base() {
    let got = scan(&[
        ("x/zombie/store.rs", "pub fn dir() -> PathBuf {\n marspot::paths::state_root().join(\"zombie\")\n}\nfn new_file() { dir().join(\"fresh.tsv\"); }"),
        ("x/zombie/teller.rs", "fn t() { let d = store::dir(); d.join(\"told2.tsv\"); }"),
        ("x/up.rs", "fn cache_dir() -> PathBuf { state_root() }\nfn p() { cache_dir().join(\"download/x.tar\"); }"),
        ("x/pb.rs", "fn private_dir() -> io::Result<PathBuf> {\n let dir = state_root().join(\"peerbus\");\n mk(&dir)?;\n Ok(dir)\n}\nfn q() { let d = private_dir()?; d.join(\"z.sock\"); }"),
    ]);
    assert_eq!(got, ["download/x.tar", "peerbus", "peerbus/z.sock", "zombie", "zombie/fresh.tsv", "zombie/told2.tsv"]);
}

#[test]
fn a_path_passed_to_a_function_or_closure_is_followed() {
    let got = scan(&[
        ("x/alloc.rs", "fn events_dir(root: &Path) -> PathBuf { root.join(\"plugins\").join(\"gemini\").join(\"alloc_events\") }\nfn w() { events_dir(&state_root()).join(\"x.tsv\"); }"),
        (
            "x/sb.rs",
            "impl Scrollback { pub fn file(bin_path: PathBuf, cols: usize) -> R {\n let quarantine = |p: &Path| {\n let mut q = p.as_os_str().to_owned();\n q.push(format!(\".corrupt-{ts}\"));\n };\n quarantine(&bin_path);\n } }",
        ),
        ("x/new.rs", "fn bin(id: u64) -> PathBuf { session_dir(id).join(\"scrollback.bin\") }\nfn n() { Scrollback::file(bin(3), 80); }"),
    ]);
    assert_eq!(
        got,
        ["plugins/gemini/alloc_events", "plugins/gemini/alloc_events/x.tsv", "sessions/{}/scrollback.bin", "sessions/{}/scrollback.bin.corrupt-{}"]
    );
}

#[test]
fn computed_names_are_followed() {
    let got = scan(&[(
        "x/f.rs",
        "fn a() {\n let root = state_root().join(\"retired\");\n root.join(format!(\"{id}-{}\", ts));\n let t = state_root().join(\"zombie/told.tsv\");\n t.with_extension(\"tsv.new\");\n state_root().join(\"sb/scrollback.bin\").with_extension(\"cold.bin\");\n state_root().join(\"usage/a.json\").with_file_name(\"b.json\");\n let mut p = state_root().join(\"handoff\");\n p.push(\"7.md\");\n}",
    )]);
    assert_eq!(got, ["handoff", "handoff/7.md", "retired", "retired/{}-{}", "sb/scrollback.cold.bin", "usage/b.json", "zombie/told.tsv", "zombie/told.tsv.new"]);
}

#[test]
fn constants_comments_and_shared_names_are_read_right() {
    let got = scan(&[
        ("x/a.rs", "const A: &'static str = \"a.tsv\";\nfn f() {\n state_root() // a note\n .join(A);\n}"),
        ("x/b.rs", "pub const NAME: &str = \"one.tsv\";"),
        ("x/c.rs", "pub const NAME: &str = \"two.tsv\";"),
        ("x/d.rs", "fn g() { state_root().join(NAME); }"),
    ]);
    assert_eq!(got, ["a.tsv", "one.tsv", "two.tsv"]);
}

#[test]
fn a_hole_is_registered_when_a_placeholder_takes_it() {
    assert!(registered(&format!("sessions/{HOLE}/scrollback.bin")));
    assert!(registered(&format!("plugins/claudecode/alloc_events/{HOLE}.tsv")));
    assert!(!registered(&format!("zombie/{HOLE}.unheard")));
    assert!(registered("usage/claude-usage.tmp") && registered("pane-codes.tmp") && registered("autorun.tsv.tmp"));
    assert!(registered(&format!("plugins/claudecode/model/.{HOLE}.tmp")) && registered(&format!("handoff/{HOLE}.tmp")));
    assert!(!registered("zombie/unheard.tmp"));
}
