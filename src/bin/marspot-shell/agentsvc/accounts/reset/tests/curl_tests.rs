//! The devops key reaches curl only through its stdin: the verify GET
//! is run through a stand-in curl that writes down its arguments, its
//! environment and its open descriptors, then hands the request to the
//! real curl against a local devops stand-in.

use crate::plugins::usage::http;

use super::*;

const KEY: &str = "dk_secret_curl_7";

/// A stand-in curl writing what it was given under `dir`.
fn recording_curl(dir: &marspot_term::test_env::TempDir) -> std::path::PathBuf {
    let d = dir.path().display();
    let body = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{d}/argv'\n/usr/bin/env > '{d}/env'\n/bin/ls /dev/fd > '{d}/fds'\nexec /usr/bin/curl \"$@\"\n"
    );
    let bin = dir.join("curl");
    std::fs::write(&bin, body).unwrap();
    let mut p = std::fs::metadata(&bin).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o755);
    std::fs::set_permissions(&bin, p).unwrap();
    bin
}

fn fds_in(list: &str) -> Vec<i32> {
    list.split_whitespace().filter_map(|f| f.parse().ok()).collect()
}

#[test]
fn reset_the_devops_key_is_in_neither_curl_s_arguments_nor_its_environment() {
    let dir = marspot_term::test_env::TempDir::new("reset-curl-argv");
    let curl = recording_curl(&dir);
    // a descriptor of ours without close-on-exec, far above what sh opens
    // SAFETY: plain descriptor calls on a descriptor this test owns
    let leaked = unsafe { libc::fcntl(2, libc::F_DUPFD, 200) };
    assert!(leaked >= 200);

    // the probe can fail: a child started plainly does see that descriptor
    let plain = std::process::Command::new(&curl).arg("--version").output().unwrap();
    assert!(plain.status.success());
    assert!(fds_in(&std::fs::read_to_string(dir.join("fds")).unwrap()).contains(&leaked), "the stand-in lists inherited descriptors");

    let (base, server) = devops(vec![(200, CONFIRMED.into())]);
    let access = verify::Access { base, key: KEY.into() };
    let c = claim(Provider::Codex, 2, "cc-1");
    let g = http::with_curl(curl, || verify::verify(&access, &c, "tok-c"));
    assert_eq!(g.map(|g| g.ends_at), Ok(Some(1_796_070_363)), "the real verify path went through the stand-in");
    let heads = server.join().unwrap();
    assert!(heads[0].contains(&format!("Bearer {KEY}")), "the key still reached devops: {}", heads[0]);

    let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(argv.contains("--max-time\n20\n") && argv.ends_with("-K\n-\n"), "{argv}");
    assert!(!argv.contains(KEY), "{argv}");
    assert!(!env.contains(KEY), "{env}");
    assert!(!env.lines().any(|l| l.starts_with("HOME=") || l.starts_with("USER=")), "curl gets an empty environment: {env}");
    let fds = fds_in(&std::fs::read_to_string(dir.join("fds")).unwrap());
    assert!(!fds.contains(&leaked), "no descriptor of ours but stdio crosses: {fds:?}");
    // SAFETY: closing the descriptor duplicated above
    unsafe { libc::close(leaked) };
}
