//! Replacing an agent's settings file whole, through the link to it.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One writer at a time in this process: the shared-settings watch, the
/// status-line hook and the peer-bus config writers can all reach the
/// same file from different threads.
static WRITING: Mutex<()> = Mutex::new(());
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Replace the file's contents, keeping it the same file.
///
/// The profiles' `settings.json` are symlinks to one shared file;
/// renaming onto the link path would replace the link itself, so the
/// caller passes the canonical path and the temp file is made beside
/// it.  Each call has a temp file of its own, written to disk before the
/// rename, so a reader of the file only ever sees one whole text.
pub(crate) fn write_through_symlink(path: &Path, text: &str) -> std::io::Result<()> {
    let _one = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    let dir = path.parent().unwrap_or(Path::new("."));
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".marspot-settings-{}-{n}.tmp", std::process::id()));
    super::model_guard::note_own_write(path, text);
    let written = write_synced(&tmp, path, text).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn write_synced(tmp: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(tmp)?;
    if let Ok(md) = fs::metadata(path) {
        f.set_permissions(md.permissions())?;
    }
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_on_many_threads_each_leave_a_whole_text_and_no_temp_file() {
        let d = std::env::temp_dir().join(format!("marspot-settings-write-{}", marspot_peer::rand::hex(6)));
        fs::create_dir_all(&d).unwrap();
        let file = d.join("settings.json");
        fs::write(&file, "{}").unwrap();
        let texts: Vec<String> = (0..8).map(|i| format!("{{\n  \"n\": {i},\n  \"pad\": \"{}\"\n}}\n", "x".repeat(64 * 1024))).collect();
        std::thread::scope(|s| {
            for t in &texts {
                let file = &file;
                s.spawn(move || {
                    for _ in 0..20 {
                        write_through_symlink(file, t).unwrap();
                    }
                });
            }
            let reader = s.spawn(|| {
                for _ in 0..400 {
                    let now = fs::read_to_string(&file).unwrap();
                    assert!(now == "{}" || texts.contains(&now), "a text no writer wrote: {} bytes", now.len());
                }
            });
            reader.join().unwrap();
        });
        let left: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("settings.json")], "no temp file is left behind");
        fs::remove_dir_all(&d).unwrap();
    }
}
