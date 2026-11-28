use super::*;
use crate::test_env::TempDir;
use std::io::{Read, Write};
use std::path::PathBuf;

/// A socket path of exactly `len` bytes inside `dir`.
fn path_of_len(dir: &TempDir, len: usize) -> PathBuf {
    let base = dir.path().as_os_str().len() + 1;
    assert!(len > base + 8, "the temp dir is too long for a {len}-byte path");
    let mut p = dir.path().to_path_buf();
    let mut left = len - base;
    // directories of at most 64 bytes, then the socket's own name
    while left > 64 {
        p.push("d".repeat(63));
        left -= 64;
    }
    std::fs::create_dir_all(&p).unwrap();
    p.push("s".repeat(left));
    assert_eq!(p.as_os_str().len(), len);
    p
}

fn round_trip(path: &Path) {
    let listener = bind(path).unwrap_or_else(|e| panic!("bind {}: {e}", path.display()));
    let mut client = connect(path).unwrap_or_else(|e| panic!("connect {}: {e}", path.display()));
    let (mut server, _) = listener.accept().unwrap();
    client.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    server.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping");
}

#[test]
fn uds_path_binds_and_connects_on_both_sides_of_what_std_takes() {
    let dir = TempDir::new("uds-len");
    for len in [STD_MAX, STD_MAX + 1, 160] {
        round_trip(&path_of_len(&dir, len));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn uds_path_takes_the_longest_path_the_kernel_takes() {
    let dir = TempDir::new("uds-max");
    round_trip(&path_of_len(&dir, KERNEL_MAX));
}

#[test]
fn uds_path_names_a_path_too_long_for_any_socket_address() {
    let dir = TempDir::new("uds-over");
    let path = path_of_len(&dir, KERNEL_MAX + 1);
    for err in [bind(&path).unwrap_err(), connect(&path).unwrap_err()] {
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains(&format!("{} bytes", KERNEL_MAX + 1)), "{msg}");
    }
}

#[test]
fn uds_path_connect_to_a_missing_long_socket_is_not_found() {
    let dir = TempDir::new("uds-missing");
    let err = connect(path_of_len(&dir, 150)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}
