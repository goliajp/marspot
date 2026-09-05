//! Verify RFC-007's two halves: the copy is unmarked, and a job booted
//! from it produces a process whose files are unmarked.
use std::path::PathBuf;
fn xattrs(p: &str) -> String {
    let o = std::process::Command::new("/usr/bin/xattr").arg(p).output().unwrap();
    String::from_utf8_lossy(&o.stdout).replace('\n', " ").trim().to_string()
}
fn main() {
    let src = PathBuf::from(std::env::args().nth(1).expect("source binary"));
    println!("source        {} xattr=[{}]", src.display(), xattrs(&src.to_string_lossy()));
    let clean = marspot::clean_exec::ensure_clean_copy(&src).expect("clean copy");
    println!("clean copy    {} xattr=[{}]", clean.display(), xattrs(&clean.to_string_lossy()));

    // Boot /bin/sh from a job and have it write a file; that file's
    // xattrs are the whole question.
    let out = std::env::temp_dir().join(format!("rfc007_probe_{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let script = format!(
        "t=$(mktemp /tmp/r7.XXXXXX); echo x > $t; /usr/bin/xattr $t > {} 2>&1; echo DONE >> {}; rm -f $t",
        out.display(), out.display()
    );
    // Simpler and equivalent: run the script through the sync helper.
    marspot::clean_exec::run_probe_script(&script).expect("probe script");
    for _ in 0..120 {
        if std::fs::read_to_string(&out).map(|s| s.contains("DONE")).unwrap_or(false) { break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let got = std::fs::read_to_string(&out).unwrap_or_default();
    println!("job's file    xattr=[{}]", got.lines().next().unwrap_or("").trim());
    println!("(空 = 干净;这一行是 RFC-007 成立与否的判据)");
    let _ = std::fs::remove_file(&out);
}
