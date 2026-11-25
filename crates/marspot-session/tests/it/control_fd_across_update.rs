//! The variable naming the control socket a core hands a cold-started L3
//! does not cross its execv.  The new image takes the stream from the
//! handoff; the variable is the cold image's alone.

use std::os::fd::AsRawFd;

use marspot_term::shell_proto::MsgType;

use crate::self_execv::{Sandbox, count_in_log, stage, start, wait_for};

/// `lsof -F` fields of the unix sockets `pid` holds, as (fd, device,
/// name) per descriptor.
fn unix_sockets(pid: u32) -> Vec<(String, String, String)> {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-a", "-U", "-p", &pid.to_string(), "-F", "fdn"])
        .output()
        .expect("run lsof");
    let mut fds = Vec::new();
    let mut cur: Option<(String, String, String)> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let (tag, rest) = line.split_at(1.min(line.len()));
        match tag {
            "f" => {
                fds.extend(cur.take());
                cur = Some((rest.to_string(), String::new(), String::new()));
            }
            "d" => cur.iter_mut().for_each(|c| c.1 = rest.to_string()),
            "n" => cur.iter_mut().for_each(|c| c.2 = rest.to_string()),
            _ => {}
        }
    }
    fds.extend(cur);
    fds
}

/// How many descriptors of `pid` are the L3's end of the test's control
/// socket: the socket whose peer is the test's end.
fn control_fds(pid: u32, ours: i32) -> usize {
    let me = unix_sockets(std::process::id());
    let (_, _, name) = me
        .iter()
        .find(|(fd, _, _)| *fd == ours.to_string())
        .expect("lsof lists the test's end of the control socket");
    let peer = name.strip_prefix("->").expect("the test's end is connected");
    unix_sockets(pid).iter().filter(|(_, dev, _)| dev == peer).count()
}

#[test]
fn the_cold_control_fd_does_not_cross_an_update() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let pid = s.pid();
    let ours = s.control.as_raw_fd();
    let cold = control_fds(pid, ours);
    // the stream the reader owns and the writer's duplicate
    assert!(cold >= 2, "lsof sees the cold image's control socket: {cold}");

    stage(&state, true);
    let before = count_in_log(&state, "SESSION_OWNER");
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(20, "the new image to finish its boot", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited");
        (count_in_log(&state, "SESSION_OWNER") > before).then_some(())
    });
    assert_eq!(count_in_log(&state, "L3_EXECV_CONTROL_ADOPTED"), 1, "the handoff carried the stream");
    // the cold stream is closed with its reader before the execv today;
    // this guards against keeping it past the execv later
    assert_eq!(
        control_fds(pid, ours),
        cold,
        "the new image holds the handed-over stream and its writer, nothing from the cold image"
    );
    assert_eq!(
        count_in_log(&state, "session.control_fd.env_on_resume"),
        0,
        "the variable naming the cold fd did not reach the new image"
    );
    s.kill();
}
