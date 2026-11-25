use std::fs::{self, OpenOptions};
use std::io::Write as _;

use super::*;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("zombie-tail-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Dir(d)
}

fn append(p: &Path, s: &str) {
    OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
}

fn read(t: &mut Tail) -> Vec<String> {
    let mut out = Vec::new();
    t.read(|l| match l {
        Line::Restart => out.push("<restart>".into()),
        Line::Text(s) => out.push(s.to_string()),
    })
    .unwrap();
    out
}

#[test]
fn zombie_tail_reads_only_what_was_appended_and_holds_a_partial_line() {
    let d = dir("append");
    let p = d.0.join("t.jsonl");
    append(&p, "a\nb\n");
    let mut t = Tail::new(&p);
    assert_eq!(read(&mut t), ["<restart>", "a", "b"], "a small file is read from the top");
    append(&p, "c\npart");
    assert_eq!(read(&mut t), ["c"]);
    append(&p, "ial\n");
    assert_eq!(read(&mut t), ["partial"]);
    assert!(read(&mut t).is_empty());
    assert_eq!(t.read_bytes, 14);
}

#[test]
fn zombie_tail_starts_over_after_a_cut_and_after_a_new_file() {
    let d = dir("cut");
    let p = d.0.join("t.jsonl");
    append(&p, "one\ntwo\n");
    let mut t = Tail::new(&p);
    read(&mut t);
    fs::write(&p, "x\n").unwrap();
    assert_eq!(read(&mut t), ["<restart>", "x"], "cut shorter");
    let q = d.0.join("new.jsonl");
    fs::write(&q, "fresh\nlines\nmore\n").unwrap();
    fs::rename(&q, &p).unwrap();
    assert_eq!(read(&mut t), ["<restart>", "fresh", "lines", "more"], "another inode under the same name");
}

#[test]
fn zombie_tail_of_a_big_file_reads_only_its_last_64_kib() {
    let d = dir("big");
    let p = d.0.join("t.jsonl");
    let line = format!("{}\n", "x".repeat(99));
    let mut body = String::new();
    for _ in 0..2000 {
        body.push_str(&line);
    }
    body.push_str("last\n");
    fs::write(&p, &body).unwrap();
    let mut t = Tail::new(&p);
    let got = read(&mut t);
    assert_eq!(got[0], "<restart>");
    assert_eq!(got.last().map(String::as_str), Some("last"));
    assert!(got[1..got.len() - 1].iter().all(|l| l.len() == 99), "the partial first line is dropped");
    assert!(t.read_bytes <= FIRST);
}
