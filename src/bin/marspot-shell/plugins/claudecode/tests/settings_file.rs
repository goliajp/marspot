//! The status-line hook as an edit of the user's Claude Code settings
//! file: put in, repointed, handed back, and never more than that.

use super::*;

/// Registering the hook with Claude Code, on and off again.
///
/// The file is the user's, so what is asserted is not just that
/// the key appears and disappears but that everything around it
/// comes back byte for byte — across the shapes a settings.json
/// actually turns up in.
#[test]
fn the_hook_registers_and_unregisters_leaving_the_file_as_it_was() {
    let tmp = TempDir::new("cc-statusline-reconcile-test");
    let dir = tmp.path().to_path_buf();
    let bin = std::path::Path::new("/Apps/Marspot.app/Contents/MacOS/marspot-shell");

    let shapes = [
        // One line, several members.
        "{\"a\":1,\"b\":{\"c\":2}}",
        // Indented, statusLine would not be the last member.
        "{\n  \"a\": 1,\n  \"b\": { \"c\": 2 }\n}\n",
        // Indented, one member — so the comma to remove is the one
        // in front, not the one behind.
        "{\n  \"a\": 1\n}\n",
    ];
    for (i, before) in shapes.iter().enumerate() {
        let path = dir.join(format!("settings{i}.json"));
        fs::write(&path, before).unwrap();
        let files = vec![path.clone()];

        // Off and nothing of ours present: not one byte touched.
        assert!(reconcile_statusline_in(false, Some(bin), &files).is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), *before);

        let notes = reconcile_statusline_in(true, Some(bin), &files);
        assert!(notes[0].ends_with("installed"), "{notes:?}");
        let on = fs::read_to_string(&path).unwrap();
        assert!(on.contains("--cc-statusline"));
        assert!(json_parses(&on), "{on}");
        // Same again is a no-op — the switch does not rewrite the
        // file on every scan.
        assert!(reconcile_statusline_in(true, Some(bin), &files).is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), on);

        reconcile_statusline_in(false, Some(bin), &files);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            *before,
            "shape {i} did not come back as it was"
        );
    }
}

/// A status line the user already wrote is borrowed, not taken.
///
/// Claude Code allows exactly one, and the people most likely to
/// want the model in the badge are the ones who already cared
/// enough to write one.
#[test]
fn an_existing_status_line_is_chained_and_handed_back() {
    let tmp = TempDir::new("cc-statusline-chain-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("settings.json");
    let bin = std::path::Path::new("/Apps/M.app/marspot-shell");
    // Theirs has a space in it, and a quote, because both are
    // things a status line command really contains.
    let theirs = "~/bin/my line.sh --say \\\"hi\\\"";
    let before =
        format!("{{\n  \"statusLine\": {{ \"type\": \"command\", \"command\": \"{theirs}\" }},\n  \"a\": 1\n}}\n");
    fs::write(&path, &before).unwrap();
    let files = vec![path.clone()];

    let notes = reconcile_statusline_in(true, Some(bin), &files);
    assert!(notes[0].contains("chaining"), "{notes:?}");
    let on = fs::read_to_string(&path).unwrap();
    assert!(json_parses(&on), "{on}");
    let (_, cmd) = status_line_command(&on).unwrap();
    assert!(cmd.starts_with("'/Apps/M.app/marspot-shell' --cc-statusline --chain "));
    // What comes back out of `--chain` is what went in, quotes and
    // spaces intact — it is handed to `sh -c` exactly as claude
    // would have handed it.
    assert_eq!(chained_out_of(&cmd), "~/bin/my line.sh --say \"hi\"");

    reconcile_statusline_in(false, Some(bin), &files);
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}

/// An installed hook follows the binary.
///
/// `install-local` leaves the bundle's copy alone while the app
/// holds it open, so a hook registered then names
/// `binaries/current/`; the cold launch that refreshes the bundle
/// is when it should move back.
#[test]
fn an_installed_hook_is_repointed_when_the_binary_moves() {
    let tmp = TempDir::new("cc-statusline-repoint-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("settings.json");
    let files = vec![path.clone()];
    fs::write(&path, "{\n  \"a\": 1\n}\n").unwrap();

    let old = std::path::Path::new("/state/binaries/current/marspot-shell");
    reconcile_statusline_in(true, Some(old), &files);
    let new = std::path::Path::new("/Apps/Marspot.app/Contents/MacOS/marspot-shell");
    let notes = reconcile_statusline_in(true, Some(new), &files);
    assert!(notes[0].ends_with("repointed"), "{notes:?}");
    let (_, cmd) = status_line_command(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(cmd, "'/Apps/Marspot.app/Contents/MacOS/marspot-shell' --cc-statusline");
}

/// The guard that stands between an edit and the user's tool.
#[test]
fn a_settings_edit_that_would_not_parse_is_refused() {
    assert!(json_parses("{\"a\":1}"));
    assert!(json_parses("{\n  \"a\": [1, 2],\n  \"b\": {}\n}\n"));
    // The two shapes a mis-cut member leaves behind.
    assert!(!json_parses("{\"a\":1,}"));
    assert!(!json_parses("{\"a\":1,,\"b\":2}"));
    assert!(!json_parses("{\"a\":1"));
    // A brace or a comma inside a string is not structure.
    assert!(json_parses("{\"a\":\"},{\"}"));
    assert!(json_parses("{\"a\":\"\\\\\"}"));
}

/// Some JSON writers escape every `/`.  A hook already pointing at the
/// right binary reads back as that binary, so the file is left alone
/// (2026-11-09: it was rewritten unchanged once a minute, all day).
#[test]
fn a_file_whose_writer_escapes_slashes_is_read_as_what_it_says() {
    let tmp = TempDir::new("cc-statusline-escaped-test");
    let path = tmp.path().join("settings.json");
    let files = vec![path.clone()];
    let bin = std::path::Path::new("/Apps/M.app/marspot-shell");
    let before = "{\n  \"statusLine\" : {\n    \"command\" : \"'\\/Apps\\/M.app\\/marspot-shell' --cc-statusline\",\n    \"type\" : \"command\"\n  }\n}";
    fs::write(&path, before).unwrap();

    assert_eq!(status_line_command(before).unwrap().1, "'/Apps/M.app/marspot-shell' --cc-statusline");
    let notes = reconcile_statusline_in(true, Some(bin), &files);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}

/// In such a file a repoint and a hand-back still land on the value
/// itself, whatever escapes it was written with.
#[test]
fn an_escaped_value_is_replaced_and_handed_back_in_place() {
    let tmp = TempDir::new("cc-statusline-escaped-edit-test");
    let path = tmp.path().join("settings.json");
    let files = vec![path.clone()];
    let before = "{\"statusLine\":{\"command\":\"'\\/old\\/marspot-shell' --cc-statusline --chain '~\\/bin\\/mine \\u00e9'\",\"type\":\"command\"},\"a\":1}";
    fs::write(&path, before).unwrap();

    let new = std::path::Path::new("/new/marspot-shell");
    let notes = reconcile_statusline_in(true, Some(new), &files);
    assert!(notes[0].ends_with("repointed"), "{notes:?}");
    let on = fs::read_to_string(&path).unwrap();
    assert!(json_parses(&on), "{on}");
    let (_, cmd) = status_line_command(&on).unwrap();
    assert_eq!(chained_out_of(&cmd), "~/bin/mine é");
    assert!(cmd.starts_with("'/new/marspot-shell' --cc-statusline"), "{cmd}");
    assert!(on.ends_with(",\"type\":\"command\"},\"a\":1}"), "{on}");

    reconcile_statusline_in(false, Some(new), &files);
    let off = fs::read_to_string(&path).unwrap();
    assert_eq!(status_line_command(&off).unwrap().1, "~/bin/mine é");
    assert!(reconcile_statusline_in(false, Some(new), &files).is_empty());
}
