//! Unix sockets at paths longer than `sockaddr_un` declares.
//!
//! `sun_path` is declared as 104 bytes, and std refuses any longer path
//! ("path must be shorter than SUN_LEN").  The macOS kernel reads the
//! address length from `sun_len` and takes a path up to
//! `SOCK_MAXADDRLEN` (255) less the two header bytes and the NUL, so a
//! state dir under a long sandbox or a long `$HOME` still gets its
//! sockets: past what std takes, the address is built here at its real
//! length.  Past what the kernel takes, the error names the path.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// The longest path std binds or connects to: `sun_path` less the NUL.
#[cfg(target_os = "macos")]
pub const STD_MAX: usize = 103;
/// The longest path std binds or connects to: `sun_path` less the NUL.
#[cfg(not(target_os = "macos"))]
pub const STD_MAX: usize = 107;

/// The longest path the kernel takes: `SOCK_MAXADDRLEN` less `sun_len`,
/// `sun_family` and the NUL.
#[cfg(target_os = "macos")]
pub const KERNEL_MAX: usize = 252;
#[cfg(not(target_os = "macos"))]
pub const KERNEL_MAX: usize = STD_MAX;

/// `UnixListener::bind`, for any path the kernel takes.
pub fn bind(path: impl AsRef<Path>) -> io::Result<UnixListener> {
    let path = path.as_ref();
    if path.as_os_str().len() <= STD_MAX {
        return UnixListener::bind(path);
    }
    long::bind(path)
}

/// `UnixStream::connect`, for any path the kernel takes.
pub fn connect(path: impl AsRef<Path>) -> io::Result<UnixStream> {
    let path = path.as_ref();
    if path.as_os_str().len() <= STD_MAX {
        return UnixStream::connect(path);
    }
    long::connect(path)
}

fn too_long(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "socket path is {} bytes, longer than the {KERNEL_MAX} a unix socket address holds: {}",
            path.as_os_str().len(),
            path.display()
        ),
    )
}

#[cfg(not(target_os = "macos"))]
mod long {
    use super::*;

    pub(super) fn bind(path: &Path) -> io::Result<UnixListener> {
        Err(too_long(path))
    }

    pub(super) fn connect(path: &Path) -> io::Result<UnixStream> {
        Err(too_long(path))
    }
}

#[cfg(target_os = "macos")]
mod long {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A `sockaddr_un` as long as the kernel takes, aligned for it.
    struct Addr {
        buf: [u64; 32],
        len: usize,
    }

    impl Addr {
        fn new(path: &Path) -> io::Result<Addr> {
            let bytes = path.as_os_str().as_encoded_bytes();
            if bytes.len() > KERNEL_MAX {
                return Err(too_long(path));
            }
            if bytes.contains(&0) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path contains a NUL byte"));
            }
            let mut a = Addr { buf: [0; 32], len: 2 + bytes.len() + 1 };
            // SAFETY: buf is 256 plain bytes, and len <= 255 was checked above
            let raw = unsafe { std::slice::from_raw_parts_mut(a.buf.as_mut_ptr().cast::<u8>(), 256) };
            raw[0] = a.len as u8;
            raw[1] = libc::AF_UNIX as u8;
            raw[2..2 + bytes.len()].copy_from_slice(bytes);
            Ok(a)
        }

        fn ptr(&self) -> *const libc::sockaddr {
            self.buf.as_ptr().cast()
        }
    }

    fn socket() -> io::Result<OwnedFd> {
        // SAFETY: plain socket(2); the fd is owned from here on
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just opened and is owned by nothing else
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let one: libc::c_int = 1;
        // SAFETY: fcntl and setsockopt on the fd opened above, with a live c_int
        let ok = unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == 0
                && libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const one).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                ) == 0
        };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    pub(super) fn bind(path: &Path) -> io::Result<UnixListener> {
        let addr = Addr::new(path)?;
        let fd = socket()?;
        // SAFETY: addr holds addr.len initialised bytes of a sockaddr_un
        let ok = unsafe {
            libc::bind(fd.as_raw_fd(), addr.ptr(), addr.len as libc::socklen_t) == 0
                && libc::listen(fd.as_raw_fd(), libc::SOMAXCONN) == 0
        };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(UnixListener::from(fd))
    }

    pub(super) fn connect(path: &Path) -> io::Result<UnixStream> {
        let addr = Addr::new(path)?;
        let fd = socket()?;
        // SAFETY: addr holds addr.len initialised bytes of a sockaddr_un
        if unsafe { libc::connect(fd.as_raw_fd(), addr.ptr(), addr.len as libc::socklen_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UnixStream::from(fd))
    }
}

#[cfg(test)]
#[path = "uds_path_tests.rs"]
mod tests;
