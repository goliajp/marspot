//! Creating, opening and mapping a grid region.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering, fence};

use super::ident::{self, Identity};
use super::{
    HEADER_BYTES, Header, MAGIC, MAX_CELLS, MAX_ROWS, VERSION, VERSION_MIN_COMPAT, capacity_bytes,
    fits_capacity, region_len, wrapped_offset,
};
use crate::grid::Cell;

/// Bytes of a region created by this build: the frame's capacity, then
/// whose region it is. A region from a build before identities ends at
/// the capacity; whatever its last page has past that is where a later
/// writer puts the identity, and nothing older reads there.
const fn region_bytes() -> usize {
    capacity_bytes() + ident::BYTES
}

/// Unique-per-process shm name. macOS caps shm names at ~31 bytes
/// including the leading '/'; `/msp-g-<pid>-<n>` stays well under.
fn next_shm_name() -> std::ffi::CString {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::ffi::CString::new(format!("/msp-g-{}-{}", std::process::id(), n)).unwrap()
}

/// Create, size and stamp a region under `name`, which must be free.
fn create(cols: u16, rows: u16, name: &CStr, owner: Option<Identity>) -> io::Result<OwnedFd> {
    assert!(cols > 0 && rows > 0, "grid_shm: zero dimension");
    assert!(
        fits_capacity(cols, rows),
        "grid_shm: {cols}x{rows} exceeds capacity cap {MAX_CELLS} cells"
    );
    // Always the full capacity, so a later resize within the cap is a
    // header change, not a remap; lazily faulted, so only the live
    // dims are resident.
    let len = region_bytes();
    let fd = unsafe {
        libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // ftruncate zero-fills, so seq starts at 0: even, never published
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
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let h = base as *mut Header;
        (*h).magic = MAGIC;
        (*h).version = VERSION;
        (*h).cell_size = std::mem::size_of::<Cell>() as u32;
        (*h).cols = cols as u32;
        (*h).rows = rows as u32;
        if let Some(owner) = owner {
            ident::stamp((base as *mut u8).add(capacity_bytes()), owner, ident::new_serial());
        }
        libc::munmap(base, len);
    }
    Ok(fd)
}

/// Create a region for `owner` under `name` and leave the name in
/// place, so another process can open it: a core started after this
/// one finds a surviving session's frames by it.
///
/// A region already under that name is a previous life of the same
/// session, since the name is unique to the session; its name is
/// taken and the processes still mapping it keep their mapping.
pub fn create_region_named(
    cols: u16,
    rows: u16,
    name: &CStr,
    owner: Identity,
) -> io::Result<OwnedFd> {
    unsafe { libc::shm_unlink(name.as_ptr()) };
    create(cols, rows, name, Some(owner))
}

/// Create a region that only the holders of its fd can map: the name
/// is removed at once.
///
/// The creator stamps the header, so the writer
/// ([`super::GridShmWriter::from_fd`]) and any reader
/// ([`super::GridShmReader::from_fd`]) can map the same fd in either
/// order.
pub fn create_region(cols: u16, rows: u16) -> io::Result<OwnedFd> {
    let name = next_shm_name();
    let fd = create(cols, rows, &name, None);
    unsafe { libc::shm_unlink(name.as_ptr()) };
    fd
}

/// Open an existing region by name, read-write so the caller can map
/// it as either end.
pub fn open_region(name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Best-effort `shm_unlink` of a region by name, so the kernel frees
/// the memory once every holder has closed it.
pub fn delete_region(name: &CStr) {
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

/// A mapped grid region.
pub(super) struct Mapped {
    pub(super) base: *mut u8,
    pub(super) len: usize,
    pub(super) cols: u16,
    pub(super) rows: u16,
}

/// The identity block of a mapping of `len` bytes, when it reaches it.
pub(super) fn ident_at(base: *mut u8, len: usize) -> Option<*mut u8> {
    (len >= region_bytes()).then(|| unsafe { base.add(capacity_bytes()) })
}

/// How much of the region behind `fd` to map: through the identity
/// when the region reaches it.
pub(super) fn mapping_len(fd: RawFd) -> io::Result<usize> {
    Ok(if size_of_region(fd)? >= region_bytes() { region_bytes() } else { capacity_bytes() })
}

fn size_of_region(fd: RawFd) -> io::Result<usize> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_size as usize)
}

/// Map a region read-write and, if its layout is an older one this
/// build still takes, bring it to the current `VERSION` in place.
/// Both ends attach through this, so an update across a layout change
/// loses nothing whichever side attaches first.
///
/// A v2 region had no per-row wrap flags: it is grown to the current
/// capacity, the flags zeroed, and the version stored last.  Anything
/// older than `VERSION_MIN_COMPAT` or newer than `VERSION` is refused,
/// as is a region of another magic, cell size or impossible dims.
pub(super) fn attach_and_maybe_upgrade(fd: RawFd) -> io::Result<Mapped> {
    let cur_size = size_of_region(fd)?;
    if cur_size < HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_shm: region smaller than header",
        ));
    }
    let want_size = capacity_bytes();
    // without the full capacity the wrap flags cannot be written
    if cur_size < want_size && unsafe { libc::ftruncate(fd, want_size as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let len = mapping_len(fd)?;
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let base = base as *mut u8;

    let h = base as *const Header;
    let (magic, version, cell_size, cols, rows) =
        unsafe { ((*h).magic, (*h).version, (*h).cell_size, (*h).cols, (*h).rows) };
    let bad = magic != MAGIC
        || !(VERSION_MIN_COMPAT..=VERSION).contains(&version)
        || cell_size != std::mem::size_of::<Cell>() as u32
        || region_len(cols as u16, rows as u16) > capacity_bytes();
    if bad {
        unsafe {
            libc::munmap(base as *mut libc::c_void, len);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_shm: incompatible region (magic/version/cell-size/dims)",
        ));
    }

    if version < VERSION {
        unsafe {
            let wrapped = base.add(wrapped_offset());
            std::ptr::write_bytes(wrapped, 0, MAX_ROWS);
            // the zeroed flags are visible before another process
            // observes the new version
            fence(Ordering::Release);
            let h = base as *mut Header;
            (*h).version = VERSION;
        }
    }

    Ok(Mapped { base, len, cols: cols as u16, rows: rows as u16 })
}

/// Whose region the one under `name` says it is, when there is one
/// and it says.
pub(super) fn identity_under(name: &CStr) -> Option<Identity> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = mapping_len(fd.as_raw_fd()).ok()?;
    let base =
        unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if base == libc::MAP_FAILED {
        return None;
    }
    let found = ident_at(base as *mut u8, len).and_then(|at| unsafe { ident::read(at) });
    unsafe { libc::munmap(base, len) };
    found
}

#[cfg(test)]
#[path = "region_tests.rs"]
mod tests;
