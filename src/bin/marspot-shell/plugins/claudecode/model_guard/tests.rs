//! The watch on real files in a scratch directory, linked the way the
//! profiles link the shared file.

use super::decide::Seen;
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

fn read(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap()
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
    assert_eq!(read(&file), OPUS);
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is still a link");
    let told = take_notices();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("fable") && told[0].contains("opus"), "{told:?}");
    e.look();
    assert!(take_notices().is_empty(), "its own write back is not a change");
}

#[test]
fn putting_back_keeps_every_other_key_changed_with_the_model() {
    managed(true);
    let (link, file) = shared_file("keys", OPUS);
    let mut e = Entry::of(file.clone());
    // one write changes the model and adds a key, as two coalesced writes would
    let both = "{\n  \"model\": \"fable\",\n  \"theme\": \"dark\",\n  \"hooks\": {\"Stop\": []}\n}\n";
    replace(&link, both);
    e.look();
    assert_eq!(read(&file), both.replace("fable", "opus"));
}

#[test]
fn a_model_that_appeared_is_taken_out_and_the_file_is_never_removed() {
    managed(true);
    let (link, file) = shared_file("appeared", "{\n  \"theme\": \"dark\"\n}\n");
    let mut e = Entry::of(file.clone());
    replace(&link, FABLE);
    e.look();
    assert_eq!(read(&file), "{\n  \"theme\": \"dark\"\n}\n");
    // a file that was not there at the start: the model is taken out, the file stays
    let (_link, file) = shared_file("absent", OPUS);
    std::fs::remove_file(&file).unwrap();
    let mut e = Entry::of(file.clone());
    assert_eq!(e.seen, Seen::Absent);
    std::fs::write(&file, FABLE).unwrap();
    e.look();
    assert_eq!(read(&file), "{\n  \"theme\": \"dark\"\n}\n");
}

#[test]
fn an_unreadable_file_is_never_acted_from() {
    managed(true);
    let (link, file) = shared_file("unreadable", OPUS);
    std::fs::write(&file, [0xff, 0xfe, b'{']).unwrap();
    let mut e = Entry::of(file.clone());
    assert_eq!(e.seen, Seen::Unreadable);
    replace(&link, FABLE);
    e.look();
    assert_eq!(read(&file), FABLE, "nothing is known to go back to");
    assert!(take_notices().is_empty());
}

#[test]
fn a_truncated_file_is_waited_out_and_the_next_write_is_not_put_back_to_empty() {
    managed(true);
    let (_link, file) = shared_file("truncate", OPUS);
    let mut e = Entry::of(file.clone());
    confirmed(3);
    // the look lands between the truncate and the write, and nothing writes
    std::fs::write(&file, "").unwrap();
    e.look();
    assert_eq!(read(&file), "", "nothing is written over a file caught half way");
    assert_eq!(e.seen, Seen::of_text(OPUS.into()), "the last whole text is kept");
    assert!(take_confirmation(Instant::now()), "the confirmation is still there");
    // the write finishes with a model nobody confirmed: put back onto what is there now
    std::fs::write(&file, FABLE).unwrap();
    e.look();
    assert_eq!(read(&file), OPUS);
}

#[test]
fn a_write_that_completes_within_the_settle_is_read_whole() {
    managed(true);
    let (_link, file) = shared_file("settle", OPUS);
    let mut e = Entry::of(file.clone());
    std::fs::write(&file, "").unwrap();
    let writer = {
        let file = file.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            std::fs::write(&file, LIGHT).unwrap();
        })
    };
    e.look();
    writer.join().unwrap();
    assert_eq!(e.seen, Seen::of_text(LIGHT.into()), "the look read again until the text was whole");
    assert_eq!(take_notices().len(), 1, "and said the other key changed");
}

#[test]
fn a_confirmed_model_is_kept_once_and_the_next_is_put_back() {
    managed(true);
    let (link, file) = shared_file("confirmed", OPUS);
    let mut e = Entry::of(file.clone());
    confirmed(7);
    replace(&link, FABLE);
    e.look();
    assert_eq!(read(&file), FABLE);
    assert!(take_notices()[0].contains("fable"));
    replace(&link, OPUS);
    e.look();
    assert_eq!(read(&file), FABLE, "the confirmation was for one change");
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
    assert_eq!(read(&file), FABLE);
    assert_eq!(take_notices().len(), 1);
    managed(true);
    replace(&link, "{\n  \"model\": \"fable\",\n  \"theme\": \"light\"\n}\n");
    e.look();
    assert!(read(&file).contains("light"), "another key is not put back");
    assert_eq!(take_notices().len(), 1, "but it is said");
}

#[test]
fn marspots_own_writes_are_not_said_and_are_remembered_once() {
    managed(true);
    let (link, file) = shared_file("own", OPUS);
    let mut e = Entry::of(file.clone());
    crate::plugins::claudecode::write_through_symlink(&file, LIGHT).unwrap();
    e.look();
    assert!(take_notices().is_empty());
    assert_eq!(e.seen, Seen::of_text(LIGHT.into()));
    // the same text from someone else later is a change like any other
    replace(&link, OPUS);
    e.look();
    take_notices();
    replace(&link, LIGHT);
    e.look();
    assert_eq!(take_notices().len(), 1, "no longer taken for marspot's");
}

#[test]
fn an_own_write_carrying_an_unconfirmed_model_is_put_back() {
    managed(true);
    let (_link, file) = shared_file("own-model", OPUS);
    let mut e = Entry::of(file.clone());
    // a read-modify-write of marspot's that picked up a model someone just wrote
    let carried = "{\n  \"model\": \"fable\",\n  \"theme\": \"dark\",\n  \"hooks\": {}\n}\n";
    crate::plugins::claudecode::write_through_symlink(&file, carried).unwrap();
    e.look();
    assert_eq!(read(&file), carried.replace("fable", "opus"));
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
    while read(&file) != OPUS && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(read(&file), OPUS);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    let mut told = take_notices();
    while told.is_empty() && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
        told = take_notices();
    }
    assert_eq!(told.len(), 1, "{told:?}");
}

/// A writer that truncates and then writes, many times over, under the
/// whole watch: the file never ends empty or short of a key.
#[test]
fn the_watch_never_leaves_a_truncating_writer_with_an_empty_or_short_file() {
    managed(true);
    let (_link, file) = shared_file("truncating", OPUS);
    let _w = Watch::start(vec![file.clone()]).expect("a watch");
    for i in 0..40 {
        let text = format!("{{\n  \"model\": \"opus\",\n  \"theme\": \"dark\",\n  \"n\": {i}\n}}\n");
        let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(&file).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
        drop(f);
        std::thread::sleep(Duration::from_millis(15));
    }
    std::thread::sleep(Duration::from_millis(400));
    let last = "{\n  \"model\": \"opus\",\n  \"theme\": \"dark\",\n  \"n\": 39\n}\n";
    assert_eq!(read(&file), last);
}

#[test]
fn a_test_never_watches_the_shared_settings() {
    assert!(std::env::var_os(ENV_TEST_FILE).is_none(), "the test runner does not name a file");
    assert_eq!(watch::files(), Vec::<PathBuf>::new());
}
