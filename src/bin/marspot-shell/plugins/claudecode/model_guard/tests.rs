//! The watch on real files in a scratch directory, linked the way the
//! profiles link the shared file.

use super::watch::{Entry, Watch};
use super::*;
use marspot::settings::{CcModel, Settings};

const OPUS: &str = "{\n  \"model\": \"opus\",\n  \"theme\": \"dark\"\n}\n";
const FABLE: &str = "{\n  \"model\": \"fable\",\n  \"theme\": \"dark\"\n}\n";
const LIGHT: &str = "{\n  \"model\": \"opus\",\n  \"theme\": \"light\"\n}\n";

/// A shared file holding `text` and a profile directory whose
/// settings.json links to it; the shared file's canonical path.
fn shared_file(tag: &str, text: &str) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("marspot-cc-model-{tag}-{}", marspot_peer::rand::hex(6)));
    std::fs::create_dir_all(d.join("shared")).unwrap();
    std::fs::create_dir_all(d.join("profile-1")).unwrap();
    let file = d.join("shared/settings.json");
    std::fs::write(&file, text).unwrap();
    std::os::unix::fs::symlink(&file, d.join("profile-1/settings.json")).unwrap();
    (d.join("profile-1/settings.json"), file.canonicalize().unwrap())
}

/// What a writer that replaces the file by rename does, through the link.
fn replace(link: &Path, text: &str) {
    let real = link.canonicalize().unwrap();
    let tmp = real.with_extension("json.new");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, &real).unwrap();
}

fn managed(on: bool) {
    let cc_model = if on { CcModel::Managed } else { CcModel::Manual };
    marspot::settings::set_for_test(Settings { cc_model, ..Settings::default() });
}

#[test]
fn an_unconfirmed_model_is_put_back_through_the_link_and_said() {
    managed(true);
    let (link, file) = shared_file("putback", OPUS);
    let mut e = Entry::of(file.clone());
    replace(&link, FABLE);
    e.look();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OPUS);
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is still a link");
    let told = take_notices();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("fable") && told[0].contains("opus"), "{told:?}");
    e.look();
    assert!(take_notices().is_empty(), "its own write back is not a change");
}

#[test]
fn a_confirmed_model_is_kept_once_and_the_next_is_put_back() {
    managed(true);
    let (link, file) = shared_file("confirmed", OPUS);
    let mut e = Entry::of(file.clone());
    confirmed(7);
    replace(&link, FABLE);
    e.look();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), FABLE);
    assert!(take_notices()[0].contains("fable"));
    replace(&link, OPUS);
    e.look();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), FABLE, "the confirmation was for one change");
}

#[test]
fn a_confirmation_too_old_does_not_count() {
    shared().confirmed_at = Some(Instant::now() - CONFIRMED_FOR - Duration::from_secs(1));
    assert!(!take_confirmation(Instant::now()));
    confirmed(1);
    assert!(take_confirmation(Instant::now()));
    assert!(!take_confirmation(Instant::now()), "taken once");
}

#[test]
fn under_manual_and_for_other_keys_nothing_is_put_back() {
    managed(false);
    let (link, file) = shared_file("manual", OPUS);
    let mut e = Entry::of(file.clone());
    replace(&link, FABLE);
    e.look();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), FABLE);
    assert_eq!(take_notices().len(), 1);
    managed(true);
    replace(&link, "{\n  \"model\": \"fable\",\n  \"theme\": \"light\"\n}\n");
    e.look();
    assert!(std::fs::read_to_string(&file).unwrap().contains("light"), "another key is not put back");
    assert_eq!(take_notices().len(), 1, "but it is said");
}

#[test]
fn marspots_own_writes_are_not_said() {
    managed(true);
    let (_link, file) = shared_file("own", OPUS);
    let mut e = Entry::of(file.clone());
    note_own_write(&file, LIGHT);
    crate::plugins::claudecode::write_through_symlink(&file, LIGHT).unwrap();
    e.look();
    assert!(take_notices().is_empty());
    assert_eq!(e.text.as_deref(), Some(LIGHT));
}

/// The whole watch: a write through the link is put back well within a
/// second, with nothing polling.
#[test]
fn the_watch_puts_a_model_back_within_a_second() {
    managed(true);
    let (link, file) = shared_file("watch", OPUS);
    let _w = Watch::start(vec![file.clone()]).expect("a watch");
    let t0 = Instant::now();
    replace(&link, FABLE);
    while std::fs::read_to_string(&file).unwrap() != OPUS && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OPUS);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    let mut told = take_notices();
    while told.is_empty() && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
        told = take_notices();
    }
    assert_eq!(told.len(), 1, "{told:?}");
}
