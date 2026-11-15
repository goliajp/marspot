//! Whose region a shared region is.
//!
//! A name says where a region was put, not what is there now: another
//! program can remove a name and create its own region under it.  So a
//! region also carries the state directory and session it belongs to,
//! and a reader that finds another session's region under the name it
//! asked for takes nothing from it.
//!
//! ## Layout
//!
//! ```text
//! magic u32 | state u32 | session u64 | serial u64
//! ```
//!
//! `serial` tells one region of a session from the next one made for
//! it, which can have the same name.  The block is at the end of the
//! region, past everything a build from before
//! identities reads or writes.  It is not part of any region's
//! versioned header: such a build takes only the versions it knows and
//! would refuse a region whose version had been raised for this.
//!
//! A region without the magic is from a writer that predates it.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering, fence};

use super::names::{self, Side};
use crate::session_registry::SessionEntry;

const MAGIC: u32 = 0x4d_53_49_44; // "MSID"

/// Bytes of the identity block.
pub(super) const BYTES: usize = 24;

/// The state directory and session a region belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// [`names::state_hash`] of the state directory.
    pub state: u32,
    pub session: u64,
}

impl Identity {
    /// Session `id` of the state directory this process runs under.
    pub fn of_session(id: u64) -> Self {
        Self { state: names::state_hash(), session: id }
    }
}

/// A number no other region made on this machine has, near enough:
/// the time, the process and a count.
pub(super) fn new_serial() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()) << 40) ^ N.fetch_add(1, Ordering::Relaxed).rotate_left(56)
}

/// Write `owner` and `serial` at `at`.  The magic goes last: a reader
/// sees either no identity or a whole one.
pub(super) unsafe fn stamp(at: *mut u8, owner: Identity, serial: u64) {
    unsafe {
        std::ptr::write_unaligned(at.add(4) as *mut u32, owner.state);
        std::ptr::write_unaligned(at.add(8) as *mut u64, owner.session);
        std::ptr::write_unaligned(at.add(16) as *mut u64, serial);
        fence(Ordering::Release);
        std::ptr::write_unaligned(at as *mut u32, MAGIC);
    }
}

/// The identity at `at`, when one was written.
pub(super) unsafe fn read(at: *const u8) -> Option<Identity> {
    unsafe {
        if std::ptr::read_volatile(at as *const u32) != MAGIC {
            return None;
        }
        Some(Identity {
            state: std::ptr::read_unaligned(at.add(4) as *const u32),
            session: std::ptr::read_unaligned(at.add(8) as *const u64),
        })
    }
}

/// The serial at `at`, when an identity was written.
pub(super) unsafe fn read_serial(at: *const u8) -> Option<u64> {
    unsafe {
        (std::ptr::read_volatile(at as *const u32) == MAGIC)
            .then(|| std::ptr::read_unaligned(at.add(16) as *const u64))
    }
}

/// Open the side region `name` of `capacity` bytes for writing as
/// `owner`'s, creating it when there is none, and say so in it.
///
/// A region already there with no identity or with `owner`'s is a
/// previous image of the same session and is taken as it is.  One
/// with another identity is another session's: the name is taken from
/// it and a new region created, and whoever maps the old one keeps it.
pub(super) fn open_owned(
    name: &CStr,
    capacity: usize,
    owner: Identity,
) -> io::Result<(OwnedFd, *mut u8)> {
    for retried in [false, true] {
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = size_of(&fd)?;
        // macOS refuses to resize a shared memory object once it has a
        // size, so a region from a previous image is taken as it is
        if size == 0 {
            if unsafe { libc::ftruncate(fd.as_raw_fd(), capacity as libc::off_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else if size != capacity {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "side region of another size"));
        }
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = base as *mut u8;
        let at = unsafe { base.add(capacity - BYTES) };
        if unsafe { read(at) }.is_none_or(|found| found == owner) {
            unsafe { stamp(at, owner, 0) };
            return Ok((fd, base));
        }
        unsafe { libc::munmap(base as *mut libc::c_void, capacity) };
        if retried {
            break;
        }
        unsafe { libc::shm_unlink(name.as_ptr()) };
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "another session keeps taking this name"))
}

/// Map the side region `name` of `capacity` bytes read-only.
/// `NotFound` when there is none.
pub(super) fn map_readonly(name: &CStr, capacity: usize) -> io::Result<*const u8> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if size_of(&fd)? != capacity {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "side region of another size"));
    }
    let base = unsafe {
        libc::mmap(std::ptr::null_mut(), capacity, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(base as *const u8)
}

fn size_of(fd: &OwnedFd) -> io::Result<usize> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_size as usize)
}

/// A session's regions, as a reader asks for them: where the grid
/// region is, and whose every region found from there must be.
#[derive(Clone, Debug)]
pub struct SessionRegions {
    grid: CString,
    owner: Identity,
    /// The session writes its identity into what it publishes, so a
    /// region without one is not its own.
    stamped: bool,
}

impl SessionRegions {
    pub fn new(grid: CString, session: u64, stamped: bool) -> Self {
        Self { grid, owner: Identity::of_session(session), stamped }
    }

    /// Session `id` of this state directory, under the name unique to
    /// it, as a session that signs its regions.
    pub fn for_session(id: u64) -> Self {
        Self::new(names::session_shm_name(id), id, true)
    }

    /// Create the grid region, signed as this session's.  A region
    /// already under the name loses the name.
    pub fn create(&self, cols: u16, rows: u16) -> io::Result<OwnedFd> {
        super::region::create_region_named(cols, rows, &self.grid, self.owner)
    }

    /// What `entry`'s session publishes through.  `None` for a session
    /// that names no grid region.
    pub fn of_entry(entry: &SessionEntry) -> Option<Self> {
        if entry.shm_name.is_empty() {
            return None;
        }
        let grid = CString::new(entry.shm_name.as_str()).ok()?;
        Some(Self::new(grid, entry.id, entry.stamps_regions))
    }

    pub fn grid_name(&self) -> &CStr {
        &self.grid
    }

    pub fn owner(&self) -> Identity {
        self.owner
    }

    pub(super) fn side_name(&self, side: Side) -> Option<CString> {
        names::side_name(&self.grid, side)
    }

    /// Refuse a region carrying `found` that is not this session's.
    ///
    /// A region with no identity is taken from a session that writes
    /// none, since that is all such a session can offer.
    pub(super) fn admit(&self, found: Option<Identity>) -> io::Result<()> {
        let ours = match found {
            Some(found) => found == self.owner,
            None => !self.stamped,
        };
        if ours {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "the region is not this session's"))
        }
    }

    /// Remove the names of all three regions.  Mappings already open
    /// stay valid until they are closed.
    pub fn delete(&self) {
        delete_session_regions(&self.grid);
    }

    /// Remove each of the three names whose region still says it is
    /// this session's: what a session that moved to another grid
    /// region leaves behind, unless another program's region is under
    /// the name by now.
    pub fn delete_if_owned(&self) {
        if super::region::identity_under(&self.grid) == Some(self.owner) {
            super::region::delete_region(&self.grid);
        }
        let sides = [
            (Side::Clusters, super::clusters::CAPACITY),
            (Side::Title, super::title::CAPACITY),
        ];
        for (side, capacity) in sides {
            let Some(name) = self.side_name(side) else { continue };
            let Ok(base) = map_readonly(&name, capacity) else { continue };
            let found = unsafe { read(base.add(capacity - BYTES)) };
            unsafe { libc::munmap(base as *mut libc::c_void, capacity) };
            if found == Some(self.owner) {
                super::region::delete_region(&name);
            }
        }
    }
}

/// Remove the grid region named `grid` and the side regions named
/// after it.
pub fn delete_session_regions(grid: &CStr) {
    super::region::delete_region(grid);
    for side in [Side::Clusters, Side::Title] {
        if let Some(name) = names::side_name(grid, side) {
            super::region::delete_region(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(stamped: bool) -> SessionRegions {
        SessionRegions {
            grid: CString::new("/msp-s-00000001.4").unwrap(),
            owner: Identity { state: 1, session: 4 },
            stamped,
        }
    }

    #[test]
    fn a_region_of_another_session_or_state_directory_is_refused() {
        let ours = Identity { state: 1, session: 4 };
        for stamped in [false, true] {
            let r = regions(stamped);
            assert!(r.admit(Some(ours)).is_ok());
            assert!(r.admit(Some(Identity { state: 2, ..ours })).is_err(), "another state directory");
            assert!(r.admit(Some(Identity { session: 5, ..ours })).is_err(), "another session");
        }
    }

    #[test]
    fn a_region_without_an_identity_is_taken_only_from_a_session_that_writes_none() {
        assert!(regions(false).admit(None).is_ok());
        assert!(regions(true).admit(None).is_err());
    }

    #[test]
    fn an_identity_reads_back_and_bytes_that_are_not_one_read_as_none() {
        let mut block = [0u8; BYTES];
        assert_eq!(unsafe { read(block.as_ptr()) }, None);
        let owner = Identity { state: 0xdead_beef, session: u64::MAX - 1 };
        assert_eq!(unsafe { read_serial(block.as_ptr()) }, None);
        unsafe { stamp(block.as_mut_ptr(), owner, 77) };
        assert_eq!(unsafe { read(block.as_ptr()) }, Some(owner));
        assert_eq!(unsafe { read_serial(block.as_ptr()) }, Some(77));
        assert_ne!(new_serial(), new_serial());
    }
}
