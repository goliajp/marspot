//! `proc.stop` through the op: who may ask, what is answered, what the
//! audit log keeps.  The processes are real.

use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, ProcStopArgs};
use marspot_peer::json::{self, Value};

use super::super::caller::{CallerKey, Peer};
use super::super::conn::Conn;
use super::super::dispatch::Ctx;
use super::super::ops_proc::serve;
use super::{Config, hub_at, temp};
use crate::observe::Observation;
use crate::plugins::zombie::attrib::Known;
use crate::plugins::zombie::stop::tests::{Shell, observe, under};

fn ctx(caller: CallerKey, obs: Observation) -> Ctx {
    Ctx { caller, caller_name: "a#1".into(), obs: Arc::new(obs), cfg: Arc::new(Config::default()), view: |c| c.obs.clone() }
}

fn args(ids: Vec<String>, force: bool) -> ProcStopArgs {
    ProcStopArgs { ids, tree: true, force }
}

/// The audit lines whose `op` is proc.stop, once there are `n`.
fn lines(path: &std::path::Path, n: usize) -> Vec<Value> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let found: Vec<Value> = text.lines().filter_map(|l| json::parse(l).ok()).filter(|v| v.str_at("op") == Some("proc.stop")).collect();
        if found.len() >= n || Instant::now() > end {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pane_control_proc_stop_from_outside_the_panes_is_refused() {
    let dir = temp("proc-ext");
    let (hub, _rx) = hub_at(&dir);
    let conn = Conn::new(&hub, Peer::default(), None);
    let a = Shell::run("/bin/sleep 61 & wait");
    let kid = under(a.pid(), "sleep", 1).remove(0);
    let c = ctx(CallerKey::External, observe(&[a.pid()]));
    let r = serve(&conn, &c, &c.obs, &Known::default(), &args(vec![format!("{}@{}", kid.pid, kid.start_us)], false)).unwrap_err();
    assert_eq!(r.code, ErrorCode::NoSuchPane);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(marspot::pidtree::proc_id(kid.pid), Some(kid.id()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pane_control_proc_stop_answers_each_id_and_audits_it_and_its_kill() {
    let dir = temp("proc-audit");
    let (hub, _rx) = hub_at(&dir);
    let conn = Conn::new(&hub, Peer { pid: Some(4242), exe: "/bin/peer".into(), pane: Some(1) }, None);
    let a = Shell::run("trap '' TERM; /bin/sleep 61 & /bin/sleep 62 & wait");
    let b = Shell::run("/bin/sleep 63 & wait");
    let mine = under(a.pid(), "sleep", 2);
    let theirs = under(b.pid(), "sleep", 1).remove(0);
    let c = ctx(CallerKey::Pane(1), observe(&[a.pid(), b.pid()]));
    let ids = vec![format!("{}@{}", mine[0].pid, mine[0].start_us), format!("{}@{}", theirs.pid, theirs.start_us), "x".into()];
    let asked = Instant::now();
    let v = serve(&conn, &c, &c.obs, &Known::default(), &args(ids.clone(), true)).unwrap();
    assert!(asked.elapsed() < Duration::from_secs(1), "the answer waited for the kill: {:?}", asked.elapsed());
    let results: Vec<&str> = v.get("results").unwrap().arr().iter().filter_map(|r| r.str_at("result")).collect();
    assert_eq!(results, ["stopped", "not_yours", "refused"], "{}", v.to_json());
    assert_eq!(v.get("force").and_then(Value::bool), Some(true));
    // sigterm is ignored there, so only the kill ends it
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(marspot::pidtree::proc_id(mine[0].pid), Some(mine[0].id()));
    let got = lines(&dir.join("audit.log"), 4);
    assert_eq!(got.len(), 4, "{got:?}");
    let first = &got[0];
    assert_eq!(first.str_at("caller"), Some("a#1"));
    assert_eq!(first.get("pid").and_then(Value::i64), Some(4242));
    assert_eq!(first.get("sid").and_then(Value::i64), Some(1));
    assert_eq!(first.str_at("id"), Some(ids[0].as_str()));
    assert_eq!(first.get("target_pid").and_then(Value::i64), Some(mine[0].pid as i64));
    assert_eq!(first.str_at("start_us"), Some(mine[0].start_us.to_string().as_str()));
    assert_eq!(first.str_at("comm"), Some("sleep"));
    assert!(first.str_at("argv").is_some_and(|a| a.starts_with("/bin/sleep 6")), "{first:?}");
    assert_eq!(first.str_at("signal"), Some("STOP,TERM,CONT"));
    assert_eq!(first.str_at("result"), Some("stopped"));
    assert_eq!(first.get("suspect").and_then(Value::bool), Some(false));
    assert!(first.str_at("at").is_some_and(|t| t.ends_with('Z')));
    assert_eq!((got[1].str_at("result"), got[1].str_at("signal")), (Some("not_yours"), Some("-")));
    assert_eq!(got[2].str_at("result"), Some("refused"));
    assert_eq!((got[3].str_at("signal"), got[3].str_at("result")), (Some("KILL"), Some("killed")), "{:?}", got[3]);
    assert_eq!(got[3].str_at("id"), Some(ids[0].as_str()));
    assert_ne!(marspot::pidtree::proc_id(mine[0].pid), Some(mine[0].id()), "the kill ended it");
    assert_eq!(marspot::pidtree::proc_id(theirs.pid), Some(theirs.id()), "the other pane's is untouched");
    let _ = std::fs::remove_dir_all(&dir);
}
