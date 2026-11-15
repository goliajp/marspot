use super::*;

const MIB: usize = 1024 * 1024;

/// A buffer of `bytes` that has been written all through.
fn written(bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes];
    buf.fill(0x5a);
    buf
}

/// This process's physical footprint, in bytes.
#[cfg(target_os = "macos")]
fn resident(_: &mut [u8]) -> usize {
    // SAFETY: an all-zero rusage_info_v4 is a valid value for the
    // kernel to fill, and the cast is the C idiom for this call.
    let info = unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        );
        assert_eq!(rc, 0, "proc_pid_rusage");
        info
    };
    info.ri_phys_footprint as usize
}

/// How much of `buf` is resident, in bytes.
#[cfg(not(target_os = "macos"))]
fn resident(buf: &mut [u8]) -> usize {
    let p = page();
    let base = buf.as_mut_ptr() as usize;
    let lo = base.next_multiple_of(p);
    let hi = (base + buf.len()) & !(p - 1);
    let mut vec = vec![0u8; (hi - lo) / p];
    // SAFETY: page-aligned range inside `buf`, one byte of `vec` a page.
    let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, vec.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    vec.iter().filter(|&&b| b & 1 != 0).count() * p
}

/// Pages given back leave what is counted against the process, and
/// count again once taken back and written.
#[test]
fn given_back_pages_stop_counting_until_taken_back_and_written() {
    let mut buf = written(4 * MIB);
    let before = resident(&mut buf);
    let mut given = Given::default();
    let len = buf.len();
    assert!(given.give_back(&mut buf, 0, len));
    let after = resident(&mut buf);
    assert!(
        before.saturating_sub(after) >= 3 * MIB,
        "given back {} KiB of 4096",
        before.saturating_sub(after) / 1024
    );

    given.take_back(&mut buf);
    buf.fill(0xa5);
    let again = resident(&mut buf);
    assert!(
        again + MIB / 4 >= before,
        "written again: {} KiB, before {} KiB",
        again / 1024,
        before / 1024
    );
    assert!(buf.iter().all(|&b| b == 0xa5));
}

/// Only whole pages between what is kept and how far the buffer was
/// written go, and what may be written before taking them back is
/// what is kept.
#[test]
fn only_whole_pages_between_what_is_kept_and_what_was_written_go() {
    let p = page();
    let mut buf = written(16 * p);
    let mut given = Given::default();
    assert!(
        !given.give_back(&mut buf, p, p + p / 2),
        "no whole page between"
    );
    assert!(!given.is_given());
    assert_eq!(given.room(buf.len()), buf.len());

    let keep = p + 3;
    assert!(given.give_back(&mut buf, keep, 6 * p + 7));
    let room = given.room(buf.len());
    let base = buf.as_ptr() as usize;
    assert!(room >= keep && room < keep + p, "room {room}");
    assert_eq!((base + room) % p, 0, "from a page boundary");
    let len = buf.len();
    assert!(!given.give_back(&mut buf, keep, len), "given back once");
    given.take_back(&mut buf);
    assert!(!given.is_given());
    assert_eq!(given.room(buf.len()), buf.len());
}

/// What comes before `keep` is left as it was.
#[test]
fn what_is_kept_keeps_its_bytes() {
    let p = page();
    let mut buf: Vec<u8> = (0..32 * p).map(|i| (i % 251) as u8 + 1).collect();
    let mut given = Given::default();
    let len = buf.len();
    assert!(given.give_back(&mut buf, 4 * p, len));
    let room = given.room(buf.len());
    assert!(
        buf[..room]
            .iter()
            .enumerate()
            .all(|(i, &b)| b == (i % 251) as u8 + 1)
    );
    given.take_back(&mut buf);
}
