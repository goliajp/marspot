//! Running a program that may never answer: with a time limit, its
//! whole process group ended when the limit passes.  The processes are
//! ones started here, never a pane's.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// What is kept of each output stream: its end.
const KEEP: usize = 64 * 1024;

pub(crate) struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

fn drain(mut r: impl Read) -> String {
    let (mut kept, mut buf) = (Vec::new(), [0u8; 8192]);
    while let Ok(n) = r.read(&mut buf) {
        if n == 0 {
            break;
        }
        kept.extend_from_slice(&buf[..n]);
        if kept.len() > KEEP {
            kept.drain(..kept.len() - KEEP);
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Run `cmd` with `input` on its standard input, for `within` at most.
pub(crate) fn run(cmd: &mut Command, input: Option<&'static [u8]>, within: Duration) -> Result<Out, String> {
    let what = cmd.get_program().to_string_lossy().into_owned();
    let until = Instant::now() + within;
    cmd.process_group(0)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{what}: {e}"))?;
    if let (Some(mut w), Some(bytes)) = (child.stdin.take(), input) {
        std::thread::spawn(move || w.write_all(bytes));
    }
    let (tx, rx) = channel();
    if let Some(r) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || tx.send((false, drain(r))));
    }
    if let Some(r) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || tx.send((true, drain(r))));
    }
    drop(tx);
    let (mut stdout, mut stderr) = (String::new(), String::new());
    for _ in 0..2 {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((true, t)) => stdout = t,
            Ok((false, t)) => stderr = t,
            Err(_) => return Err(end(&mut child, &what, within)),
        }
    }
    // its output closed, it is at its end: the sleep is for one that is not
    loop {
        match child.try_wait().map_err(|e| format!("{what}: {e}"))? {
            Some(status) => return Ok(Out { ok: status.success(), stdout, stderr }),
            None if Instant::now() >= until => return Err(end(&mut child, &what, within)),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// End `child` and its group.
fn end(child: &mut Child, what: &str, within: Duration) -> String {
    // SAFETY: the group of a child not yet collected, so its id is not another's
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.wait();
    format!("{what}: no answer within {} s", within.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_command_is_ended_with_its_group_at_the_limit() {
        let t = Instant::now();
        let r = run(Command::new("/bin/sh").args(["-c", "sleep 60 & sleep 60"]), None, Duration::from_millis(300));
        assert!(r.is_err_and(|e| e.contains("no answer")));
        assert!(t.elapsed() < Duration::from_secs(10));
        let t = Instant::now();
        let r = run(Command::new("/bin/sh").args(["-c", "exec >&- 2>&-; exec sleep 60"]), None, Duration::from_millis(300));
        assert!(r.is_err_and(|e| e.contains("no answer")), "one that closed its output and stays");
        assert!(t.elapsed() < Duration::from_secs(10));
        let r = run(&mut Command::new("/bin/cat"), Some(b"hello"), Duration::from_secs(20)).unwrap();
        assert!(r.ok);
        assert_eq!(r.stdout, "hello");
        let r = run(Command::new("/bin/sh").args(["-c", "echo no >&2; exit 3"]), None, Duration::from_secs(20)).unwrap();
        assert!(!r.ok && r.stderr == "no\n");
    }
}
