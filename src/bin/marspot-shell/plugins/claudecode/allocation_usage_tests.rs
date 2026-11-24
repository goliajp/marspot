use std::time::Duration;

use super::*;

fn rec(id: &str, model: &str, out: u64, ts: &str) -> String {
    format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":100000,"output_tokens":{out}}}}}}}"#)
}

fn tokens(out: u64) -> Tokens {
    Tokens { input: 1000, cache_read: 100_000, output: out, ..Tokens::default() }
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-alloc-usage-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("s1/subagents")).unwrap();
    d
}

fn append(p: &Path, s: String) {
    let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
    std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
}

fn now() -> i64 {
    secs(SystemTime::now())
}

#[test]
fn allocation_usage_reads_only_what_was_appended_including_subagents() {
    let dir = dir("append");
    let main = dir.join("s1.jsonl");
    let sub = dir.join("s1/subagents/agent-a.jsonl");
    fs::write(&main, rec("old", "claude-opus-5-5", 500, "2026-11-24T14:00:00Z") + "\n").unwrap();
    fs::write(&sub, "").unwrap();
    let mut r = Readers::default();
    assert!(r.read(&main, now()).is_empty(), "history before the first read is not counted");
    append(&main, rec("m1", "claude-opus-5-5", 200, "2026-11-24T14:30:00Z") + "\n");
    append(&main, rec("m1", "claude-opus-5-5", 500, "2026-11-24T14:30:01Z") + "\n");
    append(&main, rec("m2", "claude-opus-5-5", 9, "2026-11-24T14:30:02Z"));
    append(&sub, rec("s1", "claude-fable-5", 100, "2026-11-24T14:30:03Z") + "\n");
    // an agent started after the session was first read is read whole
    std::thread::sleep(Duration::from_millis(20));
    fs::write(dir.join("s1/subagents/agent-b.jsonl"), rec("b1", "claude-opus-5-5", 1, "2026-11-24T14:30:04Z") + "\n").unwrap();
    let mut got = r.read(&main, now());
    got.sort_by_key(|s| s.at);
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0].tokens, tokens(200));
    assert_eq!(got[1].tokens, Tokens { output: 300, ..Tokens::default() }, "a reply counts once");
    assert_eq!((got[2].model.as_str(), got[2].at), ("claude-fable-5", 1_795_530_603));
    assert_eq!(got[3].tokens, tokens(1));
    append(&main, "\n".to_string());
    let rest = r.read(&main, now());
    assert_eq!(rest.len(), 1, "the line without its newline is read once it is complete");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn allocation_usage_scan_cost_does_not_grow_with_finished_agents() {
    let dir = dir("many");
    let main = dir.join("s1.jsonl");
    fs::write(&main, "").unwrap();
    let old = SystemTime::now() - Duration::from_secs(20 * 3600);
    for k in 0..300 {
        let p = dir.join(format!("s1/subagents/agent-{k}.jsonl"));
        fs::write(&p, rec(&format!("x{k}"), "claude-opus-5-5", 1, "2026-11-24T10:00:00Z") + "\n").unwrap();
        fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
    }
    let live = dir.join("s1/subagents/agent-live.jsonl");
    fs::write(&live, "").unwrap();
    let mut r = Readers::default();
    assert!(r.read(&main, now()).is_empty());
    assert_eq!(r.listings, 1);
    // later scans: the transcript, the directory and the one live agent
    for _ in 0..3 {
        let (l, s) = (r.listings, r.stats);
        assert!(r.read(&main, now()).is_empty());
        assert_eq!((r.listings - l, r.stats - s), (0, 3), "nothing grows with the 300 finished agents");
    }
    append(&live, rec("l1", "claude-opus-5-5", 7, "2026-11-24T14:30:00Z") + "\n");
    assert_eq!(r.read(&main, now()).len(), 1);
    // a new agent is a new listing; the finished ones are not looked at
    std::thread::sleep(Duration::from_millis(20));
    fs::write(dir.join("s1/subagents/agent-new.jsonl"), rec("n1", "claude-opus-5-5", 2, "2026-11-24T14:31:00Z") + "\n").unwrap();
    let s = r.stats;
    assert_eq!(r.read(&main, now()).len(), 1);
    assert_eq!((r.listings, r.stats - s), (2, 4));
    // fifteen hours on nothing is followed but the transcript
    let later = now() + FOLLOW_S + 60;
    assert!(r.read(&main, later).is_empty());
    let s = r.stats;
    assert!(r.read(&main, later).is_empty());
    assert_eq!(r.stats - s, 2);
    fs::remove_dir_all(&dir).unwrap();
}
