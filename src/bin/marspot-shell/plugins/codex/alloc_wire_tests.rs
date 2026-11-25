//! The pass the codex tick runs writes down what it would do and does
//! none of it: a refused pane gets its `refused` and `would_move` rows,
//! and no pty op is submitted, no home is changed, nothing is resumed.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use super::alloc_wire::{AllocPass, Seen};
use crate::observe::Observation;
use crate::plugins::claudecode::allocation_events::ymd;
use crate::plugins::{LogLevel, PluginError, PluginHost, pty_op};
use marspot::pidtree::{ProcId, ProcTable};
use marspot_term::test_env::{EnvGuard, TempDir};

#[derive(Default)]
struct Recorder {
    submitted: Mutex<Vec<&'static str>>,
    logged: Mutex<Vec<String>>,
}

impl PluginHost for Recorder {
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        Ok(std::env::temp_dir())
    }
    fn log(&self, _l: LogLevel, tag: &str, m: &str) {
        self.logged.lock().unwrap().push(format!("{tag} {m}"));
    }
    fn submit_pty_op_at(&self, _sid: u64, op: pty_op::PtyOp, _start_at: usize) -> Result<(), PluginError> {
        self.submitted.lock().unwrap().push(op.name);
        Ok(())
    }
}

fn iso(secs: i64) -> String {
    let d = ymd(secs);
    let t = secs.rem_euclid(86_400);
    format!("{}-{}-{}T{:02}:{:02}:{:02}+00:00", &d[..4], &d[4..6], &d[6..], t / 3600, t / 60 % 60, t % 60)
}

fn feed(now: i64) -> String {
    let account = |n: u8, util: f64| {
        format!(r#"{{"name":"Codex {n}","email":"cx{n}@example.invalid","status":"allowed","utilization_5h":null,"utilization_7d":{util},"reset_5h":null,"reset_7d":{},"model_limits":[]}}"#, now + 40 * 3600)
    };
    format!(r#"{{"generated_at":"{}","accounts":[{},{}]}}"#, iso(now - 60), account(1, 0.3), account(2, 0.1))
}

/// Every file under the codex homes, with a hash of what it holds.
fn homes(dir: &std::path::Path) -> Vec<(String, u64)> {
    use std::hash::{Hash, Hasher};
    let mut v: Vec<(String, u64)> = walk(dir)
        .into_iter()
        .map(|p| (p.strip_prefix(dir).unwrap().display().to_string(), p))
        .filter(|(name, _)| name.starts_with(".codex"))
        .map(|(name, p)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::fs::read(&p).unwrap().hash(&mut h);
            (name, h.finish())
        })
        .collect();
    v.sort();
    v
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] }).collect()
}

#[test]
fn codex_alloc_pass_moves_nothing() {
    let home = TempDir::new("codex-alloc-wire");
    for d in [".codex-profile-1", ".codex-profile-2"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
        std::fs::write(home.join(d).join("config.toml"), "model = \"gpt-6-astra\"\n").unwrap();
    }
    let state = home.join("state");
    let mut env = EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", &state);
    let now = crate::plugins::claudecode::rooms::unix_now();
    std::fs::create_dir_all(home.join(".local/state/devops")).unwrap();
    std::fs::write(home.join(".local/state/devops/codex-usage.json"), feed(now)).unwrap();
    let homes_before = homes(home.path());
    assert_eq!(homes_before.len(), 2, "{homes_before:?}");
    let rollout = home.join("rollout-x.jsonl");
    let refused = format!(
        r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"task_complete","turn_id":"t1","last_agent_message":null,"error":{{"message":"You've hit your usage limit.","codex_error_info":"usage_limit_exceeded"}},"completed_at":{}}}}}"#,
        iso(now - 10),
        now - 10
    );
    std::fs::write(&rollout, format!("{refused}\n")).unwrap();
    let seen = [Seen {
        sid: 11,
        codex: ProcId { pid: i32::MAX, start_us: ((now - 7200) * 1_000_000) as u64 },
        home: home.join(".codex-profile-1"),
        cwd: Some("/w/a".into()),
        cwd_rollout: Some(rollout),
        model: Some("gpt-6-astra".into()),
        profile: Some(1),
    }];
    let obs = Observation::from_parts(Instant::now(), ProcTable::from_rows(Vec::new()), Vec::new(), |_| None);
    let host = Recorder::default();
    let mut pass = AllocPass::default();
    // what the kept events say is read on a thread of its own first
    let mut waited = 0;
    while !pass.ready() {
        assert!(waited < 500, "the events were never read back");
        std::thread::sleep(std::time::Duration::from_millis(10));
        waited += 1;
    }
    pass.run(&host, &seen, &obs);
    pass.run(&host, &seen, &obs);
    let events = walk(&state.join("plugins/codex/alloc_events"));
    let text: String = events.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
    let kinds: Vec<&str> = text.lines().map(|l| l.split('\t').nth(3).unwrap()).collect();
    assert_eq!(kinds, ["bound", "busy", "refused", "would_move"], "{text}");
    assert!(text.lines().all(|l| l.split('\t').nth(1) == Some("codex")));
    assert!(text.contains("to=P2 why=out"), "{text}");
    assert!(host.submitted.lock().unwrap().is_empty(), "no pty op: {:?}", host.submitted.lock().unwrap());
    assert_eq!(homes_before, homes(home.path()), "no codex home is touched");
    assert!(host.logged.lock().unwrap().iter().any(|l| l.starts_with("codex.alloc.refused")), "{:?}", host.logged.lock().unwrap());
}
