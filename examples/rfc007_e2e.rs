//! End-to-end: boot a real `marspot-session` through RFC-007's clean
//! path and check the files IT writes.  A stamped L3 stamps its own
//! entry.toml; a clean one does not.
use std::ffi::CString;
use std::path::PathBuf;

fn xattr_of(p: &std::path::Path) -> String {
    let o = std::process::Command::new("/usr/bin/xattr").arg(p).output().unwrap();
    String::from_utf8_lossy(&o.stdout).replace('\n', " ").trim().to_string()
}

fn main() {
    let root = std::env::temp_dir().join(format!("rfc007-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: single-threaded, before anything reads it.
    unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };

    let src = PathBuf::from(
        std::env::args().nth(1).expect("usage: rfc007_e2e <marspot-session path>"),
    );
    let clean = marspot::clean_exec::ensure_clean_copy(&src).expect("clean copy");
    println!("clean L3 binary xattr = [{}]", xattr_of(&clean));

    let sid = 900_001u64;
    let shm_name = format!("/msp-rfc007-{}", std::process::id());
    let cname = CString::new(shm_name.clone()).unwrap();
    let _region = marspot_term::grid_shm::create_region_named(80, 24, &cname)
        .expect("create shm region");

    let label = marspot::clean_exec::session_job_label(sid);
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
    marspot::clean_exec::boot_session_job(&label, &clean, &env).expect("boot job");

    let entry = root.join("sessions").join(sid.to_string()).join("entry.toml");
    let mut ok = false;
    for _ in 0..200 {
        if entry.exists() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !ok {
        println!("L3 never wrote entry.toml — boot failed");
    } else {
        println!("entry.toml      xattr = [{}]", xattr_of(&entry));
        let txt = std::fs::read_to_string(&entry).unwrap_or_default();
        let pid: i32 = txt.lines().find_map(|l| l.strip_prefix("pid = ")?.trim().parse().ok()).unwrap_or(0);
        let ppid = std::process::Command::new("ps").args(["-o","ppid=","-p",&pid.to_string()])
            .output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        println!("L3 pid={pid} ppid={ppid}  (ppid=1 = launchd 起的)");
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    marspot::clean_exec::bootout_session_job(sid);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&root);
    unsafe { libc::shm_unlink(cname.as_ptr()) };
    println!("(entry.toml xattr 为空 = 整条链干净 = RFC-007 端到端成立)");
}
