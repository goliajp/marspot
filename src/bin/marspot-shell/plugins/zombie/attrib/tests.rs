use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use marspot::pidtree::{self, ProcTable};
use marspot_term::submit_receipt::{ensure_secret, token_for};

use super::*;
use crate::observe::tests::{entry, row};

const U7: &str = "11111111-2222-4333-8444-555555555555";
const OLD: &str = "32f6eecf-5377-400d-958d-1ca24f1821d7";
const STRAY: &str = "0a76d287-c435-4cc2-b9e7-a68ffca225db";

/// A real process, killed and reaped when the test ends.
pub(in crate::plugins::zombie) struct Real(Child);

impl Drop for Real {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A directory removed when the test ends.
pub(in crate::plugins::zombie) struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const WAITER: &str = "plugins::zombie::attrib::tests::a_child_left_waiting";

/// This test binary under a hard link named `name`.  The kernel keeps
/// the environment of a system binary from other processes, so the
/// children whose environment is read are this binary, waiting in
/// `a_child_left_waiting`.
fn linked(name: &str) -> (Scratch, PathBuf) {
    let exe = std::env::current_exe().unwrap();
    let dir = Scratch(exe.parent().unwrap().join(format!("zombie-attrib-{}-{name}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let bin = dir.0.join(name);
    std::fs::hard_link(&exe, &bin).unwrap();
    (dir, bin)
}

/// The child of the tests here: waits to be killed when it is that
/// child, returns at once when the suite runs it.
#[test]
fn a_child_left_waiting() {
    if std::env::var_os("MARSPOT_ZOMBIE_CHILD").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

impl Real {
    fn spawn(bin: &Path, first: &[&str], env: &[(&str, &str)]) -> Real {
        let mut c = Command::new(bin);
        c.args(first).args(["--exact", WAITER, "--nocapture"]).env_clear().env("MARSPOT_ZOMBIE_CHILD", "1");
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let real = Real(c.spawn().unwrap());
        let pid = real.0.id() as i32;
        // the environment is the child's once it has exec'd
        let end = Instant::now() + std::time::Duration::from_secs(5);
        while pidtree::proc_env_value(pid, "MARSPOT_ZOMBIE_CHILD").is_none() {
            assert!(Instant::now() < end, "the child {pid} did not exec within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        real
    }

    pub(in crate::plugins::zombie) fn sleeping(env: &[(&str, &str)]) -> (Real, Scratch) {
        let (dir, bin) = linked("waiter");
        (Real::spawn(&bin, &[], env), dir)
    }

    /// Its row as the kernel has it, with the parent `ppid`: adopted by
    /// launchd when 1.
    pub(in crate::plugins::zombie) fn row(&self, ppid: i32) -> ProcRow {
        let mut r = pidtree::proc_row(self.0.id() as i32).expect("the process is running");
        r.ppid = ppid;
        r
    }
}

/// Pane 7 (claude) with shell pid 2, pane 9 (codex) with shell pid 3,
/// both live, and `outside` beside them.
pub(in crate::plugins::zombie) fn observe(outside: Vec<ProcRow>) -> Observation {
    let mut rows = vec![row(1, 0, "launchd"), row(2, 1, "zsh"), row(4, 2, "claude"), row(3, 1, "zsh"), row(5, 3, "codex")];
    rows.extend(outside);
    let entries = vec![(entry(7, 0, 2), true), (entry(9, 0, 3), true), (entry(12, 0, 0), false)];
    Observation::from_parts(Instant::now(), ProcTable::from_rows(rows), entries, |_| None)
}

pub(in crate::plugins::zombie) fn known(transcripts: Option<PathBuf>) -> Known {
    Known {
        codex_threads: [("thread-of-pane-9".to_string(), 9)].into(),
        claude: vec![ClaudePane { sid: 7, uuid: U7.into(), transcripts }],
        helper_parents: HashMap::new(),
    }
}

fn owner_of(obs: &Observation, known: &Known, r: &ProcRow) -> Owner {
    let mut a = Attributor::default();
    a.begin(obs, known);
    a.owner(obs, r)
}

#[test]
fn a_process_under_a_pane_shell_is_that_panes_whatever_it_inherited() {
    let obs = observe(vec![row(6, 4, "node")]);
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    assert_eq!(a.owner(&obs, obs.table.get(6).unwrap()), Owner::Pane(7));
    assert_eq!(a.owner(&obs, obs.table.get(5).unwrap()), Owner::Pane(9));
    let mut reused = obs.table.get(6).unwrap().clone();
    reused.start_us += 1;
    reused.ppid = 77;
    assert_eq!(a.owner(&obs, &reused), Owner::Unknown, "a reused pid is not the pane's process");
    assert_eq!(a.env.reads(), 0);
}

#[test]
fn a_claude_pane_running_codex_exec_keeps_what_codex_left_behind() {
    // codex exec hands its children a thread no pane is bound to, beside the claude session's id
    let (p, _) = Real::sleeping(&[("CODEX_THREAD_ID", "thread-of-the-exec"), ("CLAUDE_CODE_SESSION_ID", U7)]);
    let root = p.row(1);
    let child = row(99_001, root.pid, "tail");
    let obs = observe(vec![root.clone(), child.clone()]);
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    assert_eq!(a.owner(&obs, &root), Owner::Pane(7));
    let reads = a.env.reads();
    assert_eq!(a.owner(&obs, &child), Owner::Pane(7), "a descendant goes where its root goes");
    assert_eq!(a.env.reads(), reads, "and costs no read");
}

#[test]
fn a_codex_pane_running_claude_p_keeps_what_claude_left_behind() {
    // claude -p hands its children a session id no pane holds, beside the codex thread
    let (p, _) = Real::sleeping(&[("CLAUDE_CODE_SESSION_ID", STRAY), ("CODEX_THREAD_ID", "thread-of-pane-9")]);
    let obs = observe(vec![p.row(1)]);
    assert_eq!(owner_of(&obs, &known(None), &p.row(1)), Owner::Pane(9));

    ensure_secret().expect("a secret");
    let token = token_for(9).unwrap();
    let (p, _) = Real::sleeping(&[("CLAUDE_CODE_SESSION_ID", STRAY), ("MARSPOT_SUBMIT_TOKEN", &token)]);
    let obs = observe(vec![p.row(1)]);
    assert_eq!(owner_of(&obs, &known(None), &p.row(1)), Owner::Pane(9), "the token, when no thread names the pane");
}

#[test]
fn a_token_of_a_pane_no_longer_running_names_a_closed_pane() {
    ensure_secret().expect("a secret");
    let token = token_for(12).unwrap();
    let (p, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &token)]);
    let obs = observe(vec![p.row(1)]);
    assert_eq!(owner_of(&obs, &known(None), &p.row(1)), Owner::Closed(12));
}

#[test]
fn a_closed_helpers_process_is_its_parent_panes_while_the_parent_runs() {
    ensure_secret().expect("a secret");
    let token = token_for(12).unwrap();
    let (p, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &token)]);
    let obs = observe(vec![p.row(1)]);
    let mut k = known(None);
    k.helper_parents.insert(12, 7);
    assert_eq!(owner_of(&obs, &k, &p.row(1)), Owner::Pane(7));
    k.helper_parents.insert(12, 40);
    assert_eq!(owner_of(&obs, &k, &p.row(1)), Owner::Closed(12), "a parent that is gone too leaves it the helper's");
}

#[test]
fn a_forged_token_or_no_environment_names_nothing() {
    ensure_secret().expect("a secret");
    let (forged, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", "9-0000000000000000")]);
    let (bare, _) = Real::sleeping(&[]);
    let obs = observe(vec![forged.row(1), bare.row(1)]);
    assert_eq!(owner_of(&obs, &known(None), &forged.row(1)), Owner::Unknown);
    assert_eq!(owner_of(&obs, &known(None), &bare.row(1)), Owner::Unknown);
    let gone = row(99_002, 1, "gone");
    assert_eq!(owner_of(&observe(vec![gone.clone()]), &known(None), &gone), Owner::Unknown, "unreadable");
}

#[test]
fn a_session_whose_conversation_went_on_in_a_pane_is_that_panes() {
    let scratch = Scratch(std::env::temp_dir().join(format!("zombie-attrib-{}", std::process::id())));
    let dir = scratch.0.clone();
    std::fs::create_dir_all(&dir).unwrap();
    let continued = format!(
        "{{\"type\":\"user\",\"message\":{{}},\"sessionId\":\"{OLD}\"}}\n\
         {{\"type\":\"continued-in\",\"sessionId\":\"{OLD}\",\"continuedInSessionId\":\"{U7}\"}}\n"
    );
    std::fs::write(dir.join(format!("{OLD}.jsonl")), continued).unwrap();
    std::fs::write(dir.join(format!("{U7}.jsonl")), "{\"type\":\"assistant\"}\n").unwrap();
    let (p, _) = Real::sleeping(&[("CLAUDE_CODE_SESSION_ID", OLD)]);
    let obs = observe(vec![p.row(1)]);
    assert_eq!(owner_of(&obs, &known(Some(dir.clone())), &p.row(1)), Owner::Pane(7));
    assert_eq!(owner_of(&obs, &known(None), &p.row(1)), Owner::Unknown, "not without the transcripts");
}

#[test]
fn a_session_id_that_is_not_one_is_never_a_file_name() {
    assert_eq!(pane_of_session(&known(Some(PathBuf::from("/"))).claude, "../etc/passwd"), None);
    assert_eq!(pane_of_session(&known(None).claude, U7), Some(7));
}

/// A process named `codex` whose argv[1] is `app-server`.
fn app_server(env: &[(&str, &str)]) -> (Real, Scratch) {
    let (dir, bin) = linked("codex");
    (Real::spawn(&bin, &["app-server"], env), dir)
}

#[test]
fn a_process_of_a_codex_app_server_goes_by_its_own_thread_and_one_without_a_thread_is_nobodys() {
    ensure_secret().expect("a secret");
    let token = token_for(12).unwrap();
    let (server, _dir) = app_server(&[("MARSPOT_SUBMIT_TOKEN", &token)]);
    let (tool, _) = Real::sleeping(&[("CODEX_THREAD_ID", "thread-of-pane-9"), ("MARSPOT_SUBMIT_TOKEN", &token)]);
    let (mcp, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &token)]);
    let s = server.row(1);
    assert_eq!(s.comm, "codex");
    let (tool_row, mcp_row) = (tool.row(s.pid), mcp.row(s.pid));
    let obs = observe(vec![s.clone(), tool_row.clone(), mcp_row.clone()]);
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    assert_eq!(a.owner(&obs, &tool_row), Owner::Pane(9));
    assert_eq!(a.owner(&obs, &mcp_row), Owner::Unknown, "not by the token it inherited from the server");
    assert_eq!(a.owner(&obs, &s), Owner::Closed(12), "the server itself goes by its token");
}

#[test]
fn a_process_under_something_that_is_neither_a_pane_nor_launchd_is_not_placed() {
    let (p, _) = Real::sleeping(&[("CODEX_THREAD_ID", "thread-of-pane-9")]);
    let app = row(99_003, 1, "Xcode");
    let r = p.row(app.pid);
    let obs = observe(vec![app, r.clone()]);
    assert_eq!(owner_of(&obs, &known(None), &r), Owner::Unknown);
}

#[test]
fn one_check_reads_the_environment_of_a_bounded_number_of_new_processes() {
    let rows: Vec<ProcRow> = (0..NEW_PER_CHECK as i32 + 1).map(|i| row(99_100 + i, 1, "gone")).collect();
    let obs = observe(rows.clone());
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    for r in &rows[..NEW_PER_CHECK] {
        assert_eq!(a.owner(&obs, r), Owner::Unknown);
    }
    assert_eq!(a.owner(&obs, &rows[NEW_PER_CHECK]), Owner::Later);
    assert_eq!(a.owner(&obs, &rows[0]), Owner::Unknown, "one placed this check stays placed");
    a.begin(&obs, &known(None));
    assert_eq!(a.owner(&obs, &rows[NEW_PER_CHECK]), Owner::Unknown, "the next check gets to it");
}

#[test]
fn a_process_read_in_an_earlier_check_is_placed_past_the_budget() {
    let (p, _) = Real::sleeping(&[("CODEX_THREAD_ID", "thread-of-pane-9")]);
    let mine = p.row(1);
    let mut rows: Vec<ProcRow> = (0..NEW_PER_CHECK as i32).map(|i| row(99_200 + i, 1, "gone")).collect();
    rows.push(mine.clone());
    let obs = observe(rows.clone());
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    assert_eq!(a.owner(&obs, &mine), Owner::Pane(9));
    // the bindings change, so every placement is made again
    let mut moved = known(None);
    moved.codex_threads.insert("thread-of-pane-9".into(), 7);
    a.begin(&obs, &moved);
    for r in &rows[..NEW_PER_CHECK] {
        assert_eq!(a.owner(&obs, r), Owner::Unknown);
    }
    let reads = a.env.reads();
    assert_eq!(a.owner(&obs, &mine), Owner::Pane(7), "its environment is cached, so it spends no read");
    assert_eq!(a.env.reads(), reads);
}

#[test]
fn a_placement_is_kept_across_checks_until_the_bindings_or_panes_change() {
    let (p, _) = Real::sleeping(&[("CLAUDE_CODE_SESSION_ID", STRAY)]);
    let (q, _) = Real::sleeping(&[("CODEX_THREAD_ID", "thread-of-pane-9")]);
    let (pr, qr) = (p.row(1), q.row(1));
    let obs = observe(vec![pr.clone(), qr.clone()]);
    let mut a = Attributor::default();
    a.begin(&obs, &known(None));
    assert_eq!((a.owner(&obs, &pr), a.owner(&obs, &qr)), (Owner::Unknown, Owner::Pane(9)));
    assert_eq!(a.placed(), 2);
    a.begin(&obs, &known(None));
    assert_eq!((a.owner(&obs, &pr), a.owner(&obs, &qr)), (Owner::Unknown, Owner::Pane(9)));
    assert_eq!(a.placed(), 0, "the same view: no session lookups, no stats");
    let mut two = known(None);
    two.claude.push(ClaudePane { sid: 3, uuid: "a".into(), transcripts: None });
    two.claude.push(ClaudePane { sid: 4, uuid: "b".into(), transcripts: None });
    a.begin(&obs, &two);
    a.owner(&obs, &pr);
    let mut swapped = two.clone();
    swapped.claude.reverse();
    a.begin(&obs, &swapped);
    a.owner(&obs, &pr);
    assert_eq!(a.placed(), 0, "the same panes in another order are the same view");
    let mut bound = known(None);
    bound.claude.push(ClaudePane { sid: 9, uuid: STRAY.into(), transcripts: None });
    a.begin(&obs, &bound);
    assert_eq!(a.owner(&obs, &pr), Owner::Pane(9), "a new binding places it again");
    assert_eq!(a.placed(), 1);
    let fewer = Observation::from_parts(
        Instant::now(),
        ProcTable::from_rows(obs.table.rows().to_vec()),
        vec![(entry(7, 0, 2), true), (entry(9, 0, 3), false)],
        |_| None,
    );
    a.begin(&fewer, &bound);
    a.owner(&fewer, &qr);
    assert_eq!(a.placed(), 1, "a pane that closed places it again");
}
