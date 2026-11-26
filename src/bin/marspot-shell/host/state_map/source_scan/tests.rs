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
        ("x/a.rs", "const F: &str = \"f.tsv\";\nfn p() { state_root().join(\"zombie\")\n    .join(F); }\nfn q() { old_state_root().join(\"no\"); }"),
        ("x/plugins/kimi/b.rs", "fn l(h: &dyn H) { let Ok(dir) = h.state_dir() else { return };\n let t = dir.join(REFUSED); }\nconst REFUSED: &str = \"refused.tsv\";"),
        ("x/c.rs", "fn r() { let root = paths::state_root();\n root.join(\"a\").join(name).join(\"b\"); session_dir(7).join(\"guard\"); }"),
        ("x/e.rs", "#[cfg(test)]\nmod tests;\nfn u() { state_root().join(\"after-a-test-file\"); }"),
    ]);
    assert_eq!(got, ["a/{}/b", "after-a-test-file", "plugins/kimi/refused.tsv", "sessions/{}/guard", "zombie/f.tsv"]);
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

#[test]
fn a_test_only_field_variant_or_arm_hides_nothing_after_it() {
    let got = scan(&[(
        "x/sidecar.rs",
        "struct LineExtras {\n a: u8,\n #[cfg(test)]\n probe: HashMap<u8, Vec<u8>>,\n b: u8,\n}\nimpl LineExtras { fn open() { state_root().join(\"after-a-field\"); } }\nenum E { A, #[cfg(test)] B(u8), C }\nfn g() { state_root().join(\"after-a-variant\"); }\nstruct L { #[cfg(test)] last: u8 }\nfn h(e: E) { match e { #[cfg(test)] E::B(_) => state_root().join(\"test-arm\"), _ => state_root().join(\"after-an-arm\") }; }",
    )]);
    assert_eq!(got, ["after-a-field", "after-a-variant", "after-an-arm"]);
}

#[test]
fn a_part_the_scan_cannot_read_is_a_hole_that_must_match_a_row() {
    let got = scan(&[
        ("x/zombie/store.rs", "fn load() { let d = state_root().join(\"zombie\");\n let read = |name: &str| d.join(name);\n let bad = |name: &str| d.join(name).join(\"x.unheard\"); }"),
        (
            "x/sidecar.rs",
            "fn path_for(bin: &Path, ext: &str) -> PathBuf {\n let mut p = bin.as_os_str().to_owned();\n p.push(\".\");\n p.push(ext);\n PathBuf::from(p)\n}\nfn m() { path_for(&session_dir(1).join(\"scrollback.bin\"), \"marks\"); }",
        ),
    ]);
    assert_eq!(got, ["sessions/{}/scrollback.bin", "sessions/{}/scrollback.bin.{}", "zombie", "zombie/{}", "zombie/{}/x.unheard"]);
    assert!(registered(&format!("zombie/{HOLE}")), "told.tsv and the rest fill it");
    assert!(!registered(&format!("zombie/{HOLE}/x.unheard")));
}

#[test]
fn a_root_built_by_hand_from_the_variable_is_followed() {
    let got = scan(&[(
        "x/peer/control/mod.rs",
        "pub fn socket_path() -> PathBuf {\n let root = match std::env::var_os(\"MARSPOT_STATE_DIR\") {\n Some(d) => PathBuf::from(d),\n None => PathBuf::from(std::env::var_os(\"HOME\").unwrap_or_default()).join(\"Library/Application Support/marspot\"),\n };\n root.join(\"control.sock\")\n}",
    )]);
    assert_eq!(got, ["control.sock"]);
}

#[test]
fn a_name_defined_in_several_files_reaches_the_one_in_scope() {
    let got = scan(&[
        ("x/a/store.rs", "pub fn dir() -> PathBuf { state_root().join(\"zombie\") }"),
        ("x/b/ledger.rs", "pub fn dir() -> PathBuf { state_root().join(\"handoff\") }"),
        ("x/c.rs", "use super::store::dir;\nfn f() { dir().join(\"t.tsv\"); }"),
        ("x/d.rs", "use super::ledger::{other, dir};\nfn g() { dir().join(\"u.ledger\"); }"),
        ("x/a/store/inner.rs", "use super::*;\nfn h() { dir().join(\"v.tsv\"); }"),
        ("x/e.rs", "fn k() { dir().join(\"in-no-scope\"); }"),
    ]);
    assert_eq!(got, ["handoff", "handoff/u.ledger", "zombie", "zombie/t.tsv", "zombie/v.tsv"]);
}

#[test]
fn a_source_path_under_a_left_behind_or_foreign_row_is_not_registered() {
    assert!(!registered("previous/marspot-shell"), "only older builds wrote previous/");
    assert!(!registered("binaries/x/marspot-shelld"));
    assert!(!registered("rescue-20261126/state.bin"));
    assert!(!registered(&format!("helpers/{HOLE}/state.json")), "a helper's home is the agent's, read by no build");
    assert!(registered(&format!("helpers/{HOLE}/result.md")), "a file marspot reads there has its own row");
}
