use super::*;
use crate::observe::tests::session_with;
use std::sync::Arc;

fn proc(pid: i32, ppid: i32, argv: &[&str]) -> Proc {
    let argv: Arc<[String]> = argv.iter().map(|a| a.to_string()).collect::<Vec<_>>().into();
    Proc {
        row: pidtree::ProcRow {
            pid,
            ppid,
            comm: "x".into(),
            start_unix: 0,
            start_us: pid as u64,
            pgid: pid,
            tty_dev: 1,
            tty_fg_pgid: 1,
            status: libc::SSLEEP,
        },
        argv: Some(argv),
    }
}

fn is(argv: &[&str]) -> bool {
    argv_is_gemini(&argv.iter().map(|a| a.to_string()).collect::<Vec<_>>())
}

#[test]
fn the_standalone_build_and_the_npm_install_are_both_gemini() {
    assert!(is(&["gemini"]));
    assert!(is(&["/opt/gem/bin/gemini", "-m", "flash"]));
    // the relaunched child of the standalone build
    assert!(is(&["/opt/gem/bin/gemini", "/opt/gem/bin/gemini", "-m", "flash"]));
    assert!(is(&["node", "/opt/homebrew/bin/gemini"]));
    assert!(is(&[
        "/opt/homebrew/bin/node",
        "--max-old-space-size=32768",
        "/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js",
        "-r",
        "latest"
    ]));
}

#[test]
fn a_look_alike_is_not_gemini() {
    assert!(!is(&["gemini-helper"]));
    assert!(!is(&["/usr/local/bin/gemini-helper", "gemini"]));
    assert!(!is(&["node", "/x/gemini-helper.js"]));
    assert!(!is(&["node", "/x/server.js", "gemini"]), "the script is what counts, not a later word");
    assert!(!is(&["node"]));
    assert!(!is(&["python3", "gemini"]));
    assert!(!is(&[]));
}

/// Neither of the other two plugins takes a gemini for its own:
/// both match on this name.
#[test]
fn a_gemini_process_is_not_named_claude_or_codex() {
    for argv in [&["/opt/gem/bin/gemini"][..], &["node", "/opt/homebrew/bin/gemini"][..]] {
        let p = proc(10, 1, argv);
        assert!(!matches!(p.name(), "claude" | "codex"), "{argv:?}");
    }
}

#[test]
fn the_parent_and_its_relaunched_child_are_one_agent() {
    let procs = [
        proc(10, 1, &["/opt/gem/bin/gemini", "-m", "flash"]),
        proc(11, 10, &["/opt/gem/bin/gemini", "/opt/gem/bin/gemini", "-m", "flash"]),
        proc(12, 11, &["/bin/zsh", "-c", "ls"]),
    ];
    let pane = session_with(procs.to_vec(), 10);
    let (outer, worker) = agent_in(&pane).expect("one agent");
    assert_eq!(outer.row.pid, 10, "bound to the one in front");
    assert_eq!(worker.pid, 11, "and the child is the one that works");

    // listed child first: the order of the table is not the tree
    let pane = session_with(vec![procs[1].clone(), procs[0].clone()], 10);
    let (outer, worker) = agent_in(&pane).expect("one agent");
    assert_eq!((outer.row.pid, worker.pid), (10, 11));

    let pane = session_with(vec![proc(10, 1, &["node", "/opt/homebrew/bin/gemini"])], 10);
    let (outer, worker) = agent_in(&pane).expect("one agent");
    assert_eq!((outer.row.pid, worker.pid), (10, 10), "no relaunch: one process is both");
}

#[test]
fn a_gemini_run_by_another_program_is_not_the_pane_s_agent() {
    for front in ["claude", "/usr/bin/make", "/bin/zsh"] {
        let procs = vec![
            proc(10, 1, &[front]),
            proc(11, 10, &["/bin/zsh", "-c", "gemini -p hi"]),
            proc(12, 11, &["/opt/gem/bin/gemini", "-p", "hi"]),
        ];
        assert!(agent_in(&session_with(procs, 10)).is_none(), "{front}");
    }
    let helper = vec![proc(10, 1, &["/bin/zsh"]), proc(11, 10, &["gemini-helper"])];
    assert!(agent_in(&session_with(helper, 11)).is_none());
}

#[test]
fn a_gemini_the_shell_has_put_aside_is_not_the_pane_s_agent() {
    let procs = vec![proc(10, 1, &["/opt/gem/bin/gemini"])];
    assert!(agent_in(&session_with(procs, 0)).is_none(), "suspended or in the background");
}
