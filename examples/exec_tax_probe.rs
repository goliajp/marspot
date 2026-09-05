//! Measure the first-execution tax inside a real marspot shell.
//!
//! Boots a real `marspot-session`, drives its PTY with `InjectInput`,
//! and has the shell compile a fresh binary and time its FIRST
//! execution — the thing a Gatekeeper scan is charged for.  Compare
//! against the same script run under `Terminal.app` or a notarised
//! app to see whether this terminal is paying it.
//!
//! Reading the number alone is not enough: watch `syspolicyd`'s CPU
//! across the call, or count `performScan` in its log.  A run that
//! pays shows both; a run that does not shows neither.
use std::ffi::CString;
use std::io::Write;
use std::path::PathBuf;

use marspot_term::shell_proto::{encode_inject_input, Frame, MsgType};

fn main() {
    let src = PathBuf::from(std::env::args().nth(1).expect("usage: <marspot-session> [tag]"));
    let tag = std::env::args().nth(2).unwrap_or_else(|| "run".into());
    let root = std::env::temp_dir().join(format!("rfc007-shell-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: single-threaded, before anything reads it.
    unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };

    let bin = src.clone();

    let sid = 970_000u64 + (std::process::id() as u64 % 1000);
    let shm_name = format!("/msp-r7t-{}", std::process::id());
    let cname = CString::new(shm_name.clone()).unwrap();
    let _region = marspot_term::grid_shm::create_region_named(100, 30, &cname).expect("shm");

    let mut env: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in [
        ("MARSPOT_SESSION_ID", sid.to_string()),
        ("MARSPOT_L3_OWNS_PTY", "1".to_string()),
        ("MARSPOT_SHM_NAME", shm_name.clone()),
        ("MARSPOT_STATE_DIR", root.to_string_lossy().to_string()),
    ] {
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v));
    }
    env.retain(|(k, _)| k != "MARSPOT_SHM_FD" && k != "MARSPOT_CONTROL_FD");

    let mut cmd = std::process::Command::new(&bin);
    cmd.env_clear().envs(env.iter().cloned());
    cmd.spawn().expect("spawn L3");

    let mut control = marspot_term::uds_session_client::wait_and_connect(
        sid,
        std::time::Duration::from_secs(15),
    )
    .expect("connect to L3");

    // Let the shell finish drawing its prompt before typing at it.
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let out = root.join("timing.txt");
    let script = format!(
        "{{ n=/tmp/r7t_$$_$RANDOM; printf 'int main(void){{return %d&0;}}\\n' $RANDOM | cc -O0 -x c - -o $n; \
         echo -n 'first_exec '; /usr/bin/time -p $n 2>&1 | awk '/^real/{{print $2\"s\"}}'; \
         echo \"artefact_xattr [$(/usr/bin/xattr $n | tr '\\n' ' ')]\"; rm -f $n; echo R7T_DONE; }} > {} 2>&1\r",
        out.display()
    );
    let frame = Frame::new(MsgType::InjectInput, encode_inject_input(sid, script.as_bytes()));
    frame.write_to(&mut control).expect("send InjectInput");
    control.flush().ok();

    let mut got = String::new();
    for _ in 0..600 {
        if let Ok(t) = std::fs::read_to_string(&out) {
            if t.contains("R7T_DONE") {
                got = t;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!("L3 binary xattr [{}]", {
        let o = std::process::Command::new("/usr/bin/xattr").arg(&bin).output().unwrap();
        String::from_utf8_lossy(&o.stdout).replace('\n', " ").trim().to_string()
    });
    if got.is_empty() {
        println!("(shell 没有回话 — 驱动失败,不是数据)");
    } else {
        for l in got.lines().filter(|l| !l.trim().is_empty() && *l != "R7T_DONE") {
            println!("{l}");
        }
    }

    if let Ok(e) = marspot_term::session_registry::read_session_entry(sid) {
        unsafe { libc::kill(e.pid, libc::SIGTERM) };
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&root);
    unsafe { libc::shm_unlink(cname.as_ptr()) };
}
