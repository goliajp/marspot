use super::super::{GridShmReader, GridShmWriter, SessionRegions};
use super::*;

/// A session no other test or live pane uses.
fn test_regions(n: u64) -> SessionRegions {
    let id = 0xAD_0000_0000 + u64::from(std::process::id()) * 10 + n;
    SessionRegions::for_session(id)
}

fn as_another_session(regions: &SessionRegions) -> SessionRegions {
    SessionRegions::new(regions.grid_name().to_owned(), regions.owner().session + 1, true)
}

#[test]
fn a_reader_opens_only_the_grid_region_of_its_session() {
    let regions = test_regions(0);
    let _fd = regions.create(8, 2).unwrap();
    let own = GridShmReader::open(&regions);
    let other = GridShmReader::open(&as_another_session(&regions));
    regions.delete();
    assert_eq!(own.expect("its own region").identity(), Some(regions.owner()));
    assert!(other.is_err(), "another session's grid region was mapped");
}

#[test]
fn each_region_made_for_a_session_has_its_own_serial_and_the_writer_keeps_it() {
    let regions = test_regions(1);
    let first = GridShmReader::from_fd(regions.create(8, 2).unwrap().as_raw_fd()).unwrap();
    let fd = regions.create(8, 2).unwrap();
    let second = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
    let mut writer = GridShmWriter::from_fd(fd).unwrap();
    regions.delete();
    assert!(first.serial().is_some());
    assert_ne!(first.serial(), second.serial(), "two regions under one name cannot be told apart");
    assert!(writer.stamp(regions.owner()));
    assert_eq!(writer.serial(), second.serial(), "the writer replaced its creator's serial");
}

/// A region as a core from before identities creates it: the frame's
/// capacity and nothing more.
fn region_without_identity(name: &CStr) -> OwnedFd {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = capacity_bytes();
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) }, 0);
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    unsafe {
        let h = base as *mut Header;
        (*h).magic = MAGIC;
        (*h).version = VERSION;
        (*h).cell_size = std::mem::size_of::<Cell>() as u32;
        (*h).cols = 8;
        (*h).rows = 2;
        libc::munmap(base, len);
    }
    fd
}

#[test]
fn a_region_without_an_identity_is_taken_only_from_a_session_that_writes_none() {
    let regions = test_regions(2);
    let fd = region_without_identity(regions.grid_name());
    let old_session =
        SessionRegions::new(regions.grid_name().to_owned(), regions.owner().session, false);
    let from_old = GridShmReader::open(&old_session);
    let from_new = GridShmReader::open(&regions);
    // the session's next image writes its identity where the region
    // has the room
    let mut writer = GridShmWriter::from_fd(fd).unwrap();
    let signed = writer.stamp(regions.owner());
    let after = GridShmReader::open(&regions);
    regions.delete();
    assert!(from_old.is_ok(), "{:?}", from_old.err());
    assert!(from_new.is_err(), "an unsigned region passed for a signing session's");
    assert_eq!(after.is_ok(), signed, "a region is the session's exactly when it could be signed");
}
