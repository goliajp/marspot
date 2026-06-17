//! L1 fd-vault wire protocol — RFC-003 §6 Amendment 15.
//!
//! L1 keeps a generic `HashMap<u64, (OwnedFd, Vec<u8>)>` and runs a UDS
//! server that accepts two requests:
//!
//!   * `Deposit { key, metadata }` + SCM_RIGHTS fd     — store the fd
//!   * `Withdraw { key }`  → reply with SCM_RIGHTS fd + metadata
//!
//! That is the entire surface.  L1 does NOT know:
//!   * what the key means (L2 picks it — session id, UUID, anything u64)
//!   * what the metadata bytes mean (L2 encodes whatever it wants — pid,
//!     dims, version tag, multiple fields concatenated)
//!   * what kind of kernel object the fd points at (PTY master, socket,
//!     shm fd, IOSurface fd — fd is fd)
//!
//! This deliberate ignorance is the point: L2 can grow new use-cases
//! (a fresh subsystem, a new kind of worker, a sidecar) without
//! touching L1 binary at all.  L1 is the "shell" (per user's phrasing)
//! and updating L1 always flashes the window, so it must stay small +
//! generic + rarely changed.
//!
//! Wire format reuses `shell_proto::Frame` for header framing (magic +
//! msg_type + payload length), but the msg_type values live in their
//! own range so the two protocols can't accidentally cross-talk.
//!
//! Same-uid Unix-domain socket trust — any process the user owns can
//! deposit / withdraw any key.  If you don't want L2's worker process X
//! to read worker Y's fd, don't share keys between them.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::paths::state_root;

/// Wire format version for the fd-vault protocol.  Bump when the
/// header / payload layout changes incompatibly.  Both ends MUST send
/// + verify Hello before deposit / withdraw.
pub const VAULT_PROTO_VERSION: u32 = 1;

/// Magic bytes at the start of every frame — distinct from
/// `shell_proto::MAGIC` so a misrouted UDS connection fails loudly
/// instead of confusing the two protocols.
pub const VAULT_MAGIC: u32 = 0x5641_554C; // "VAUL"

/// Hard cap on metadata payload — kept small because L1 just buffers
/// it, and an unbounded metadata is an obvious DoS surface.
pub const MAX_METADATA_LEN: usize = 64 * 1024;

/// Per-state socket path that L1 binds.  Same state dir as
/// `sessions/` so a sandbox L1 and a prod L1 don't collide.
pub fn vault_socket_path() -> PathBuf {
    state_root().join("l1-fd-vault.sock")
}

/// Env var that tells a freshly-spawned L3 (or any other resuming
/// child) where to find L1's vault socket.  Set by L1 when it spawns
/// L2 / L3, inherited down the tree.
pub const ENV_VAULT_SOCK: &str = "MARSPOT_L1_VAULT_SOCK";

/// Frame types for the fd-vault protocol.  Distinct integer space
/// from `shell_proto::MsgType` so the two never collide on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum VaultMsg {
    /// Client → L1.  Payload: `proto_version: u32 LE`.
    Hello = 1,
    /// L1 → client.  Payload: `proto_version: u32 LE`.
    HelloAck = 2,
    /// Client → L1.  Payload: `key: u64 LE + metadata_len: u32 LE +
    /// metadata: [u8; metadata_len]`.  An fd is attached via
    /// SCM_RIGHTS on the same `sendmsg` call.  L1 stores the (fd,
    /// metadata) under `key`, overwriting any prior entry under the
    /// same key (caller's choice — re-deposit is intentional re-key).
    Deposit = 3,
    /// L1 → client.  Payload empty.  Confirms the deposit landed in
    /// the vault.
    DepositAck = 4,
    /// Client → L1.  Payload: `key: u64 LE`.  L1 looks up the entry,
    /// `dup`s the fd, sends back a `Delivered` frame with metadata
    /// payload + SCM_RIGHTS fd.  The deposited entry is NOT removed
    /// — the vault keeps holding its reference so the same key can
    /// be withdrawn again on a subsequent upgrade.
    Withdraw = 5,
    /// L1 → client.  Payload: `metadata_len: u32 LE + metadata: [u8;
    /// metadata_len]`.  SCM_RIGHTS attached.
    Delivered = 6,
    /// L1 → client.  Payload: ASCII reason.  Sent on bad protocol /
    /// missing key / size overflow / etc.
    Error = 200,
}

impl VaultMsg {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => VaultMsg::Hello,
            2 => VaultMsg::HelloAck,
            3 => VaultMsg::Deposit,
            4 => VaultMsg::DepositAck,
            5 => VaultMsg::Withdraw,
            6 => VaultMsg::Delivered,
            200 => VaultMsg::Error,
            _ => return None,
        })
    }
}

const HEADER_LEN: usize = 12;

/// Header-only write (caller has already prepared `payload` and, if
/// any, the SCM_RIGHTS fd).  Use the higher-level `send_*` helpers
/// below for actual exchanges.
fn write_header<W: Write>(w: &mut W, msg: VaultMsg, payload_len: usize) -> io::Result<()> {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&VAULT_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&(msg as u32).to_le_bytes());
    header[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
    w.write_all(&header)
}

/// Read one frame's header + payload (no fd attached — fds travel
/// via separate `sendmsg(SCM_RIGHTS)` paths and are handled by the
/// `send_with_fd` / `recv_with_fd` helpers below).
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<(VaultMsg, Vec<u8>)>> {
    let mut header = [0u8; HEADER_LEN];
    match read_exact_or_eof(r, &mut header)? {
        ReadEnd::Eof => return Ok(None),
        ReadEnd::Full => {}
    }
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != VAULT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault bad magic 0x{magic:08x}, expected 0x{VAULT_MAGIC:08x}"),
        ));
    }
    let type_raw = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    if len > MAX_METADATA_LEN + 16 {
        // 16 = small slack for fixed-width key field; well-known cap so a
        // malformed sender can't trick us into giant allocations.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault payload len {len} exceeds cap"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    let msg = match VaultMsg::from_u32(type_raw) {
        Some(m) => m,
        None => {
            // Forward-compat: skip unknown msg_types like shell_proto
            // does.  Recursion is bounded by stream depth.
            return read_frame(r);
        }
    };
    Ok(Some((msg, payload)))
}

enum ReadEnd { Full, Eof }

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                if off == 0 { return Ok(ReadEnd::Eof); }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("short read: {off}/{}", buf.len()),
                ));
            }
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

// ---------------- payload codecs --------------------------------------

/// `Deposit` payload: u64 key LE + u32 metadata_len LE + metadata.
pub fn encode_deposit(key: u64, metadata: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + metadata.len());
    v.extend_from_slice(&key.to_le_bytes());
    v.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    v.extend_from_slice(metadata);
    v
}

pub fn decode_deposit(payload: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    if payload.len() < 12 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "deposit too short"));
    }
    let key = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let mlen = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    if payload.len() != 12 + mlen {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("deposit metadata_len {mlen} but payload total {}", payload.len()),
        ));
    }
    if mlen > MAX_METADATA_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("deposit metadata {mlen} exceeds cap {MAX_METADATA_LEN}"),
        ));
    }
    Ok((key, payload[12..].to_vec()))
}

/// `Withdraw` payload: u64 key LE.
pub fn encode_withdraw(key: u64) -> Vec<u8> {
    key.to_le_bytes().to_vec()
}

pub fn decode_withdraw(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "withdraw needs 8 bytes"));
    }
    Ok(u64::from_le_bytes(payload[0..8].try_into().unwrap()))
}

/// `Delivered` payload: u32 metadata_len LE + metadata.
pub fn encode_delivered(metadata: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + metadata.len());
    v.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    v.extend_from_slice(metadata);
    v
}

pub fn decode_delivered(payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "delivered too short"));
    }
    let mlen = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    if payload.len() != 4 + mlen {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("delivered metadata_len {mlen} but payload total {}", payload.len()),
        ));
    }
    Ok(payload[4..].to_vec())
}

// ---------------- SCM_RIGHTS sendmsg / recvmsg ------------------------

/// Write a frame and attach `fd` via SCM_RIGHTS on the same syscall.
/// Caller is responsible for which msg_type makes semantic sense to
/// pair with a fd (Deposit / Delivered both do).
pub fn send_frame_with_fd(
    sock: &UnixStream,
    msg: VaultMsg,
    payload: &[u8],
    fd: RawFd,
) -> io::Result<()> {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&VAULT_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&(msg as u32).to_le_bytes());
    header[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());

    // Two iovecs: header + payload.
    let mut iov = [
        libc::iovec {
            iov_base: header.as_mut_ptr() as *mut libc::c_void,
            iov_len: HEADER_LEN,
        },
        libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        },
    ];

    // Control message buffer for one fd (CMSG_SPACE for sizeof(int)).
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = iov.as_mut_ptr();
    msghdr.msg_iovlen = iov.len() as _;
    msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msghdr.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const RawFd,
            libc::CMSG_DATA(cmsg) as *mut RawFd,
            1,
        );
    }

    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msghdr, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if (n as usize) < HEADER_LEN + payload.len() {
        // Short send is theoretically possible on stream sockets but
        // very rare for sub-page payloads — surface it loudly.
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("vault short send: {n}/{}", HEADER_LEN + payload.len()),
        ));
    }
    Ok(())
}

/// Read one frame from `sock`; if it carries a SCM_RIGHTS fd, return
/// it as an OwnedFd alongside the message.  Returns `Ok((msg,
/// payload, None))` when no fd was attached, `Ok((msg, payload,
/// Some(fd)))` when one was, or `Ok((..)).Err(_)` on truncation.
pub fn recv_frame_with_fd(
    sock: &UnixStream,
) -> io::Result<(VaultMsg, Vec<u8>, Option<OwnedFd>)> {
    let mut header = [0u8; HEADER_LEN];
    let mut iov_hdr = [libc::iovec {
        iov_base: header.as_mut_ptr() as *mut libc::c_void,
        iov_len: HEADER_LEN,
    }];
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = iov_hdr.as_mut_ptr();
    msghdr.msg_iovlen = iov_hdr.len() as _;
    msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msghdr.msg_controllen = cmsg_space as _;

    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msghdr, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "vault peer closed"));
    }
    if (n as usize) < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("vault short header: {n}/{HEADER_LEN}"),
        ));
    }

    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != VAULT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault bad magic 0x{magic:08x}"),
        ));
    }
    let type_raw = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    if len > MAX_METADATA_LEN + 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault payload len {len} exceeds cap"),
        ));
    }
    let msg = match VaultMsg::from_u32(type_raw) {
        Some(m) => m,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("vault unknown msg_type {type_raw}"),
            ));
        }
    };

    // Extract attached fd, if any.  We only ever expect 0 or 1; more
    // than 1 is a protocol bug — close them and surface an error.
    let mut owned_fd: Option<OwnedFd> = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = *data.add(i);
                    if owned_fd.is_none() {
                        owned_fd = Some(<OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd));
                    } else {
                        let _ = libc::close(fd);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
        }
    }

    // Now read the payload body (separate read — we kept iov to just
    // the header so the fd ancillary lands on the same syscall).
    let mut payload = vec![0u8; len];
    if len > 0 {
        // Use the existing reader path which handles EINTR.
        let mut sock_ref = sock.try_clone()?;
        sock_ref.read_exact(&mut payload)?;
    }

    Ok((msg, payload, owned_fd))
}

/// Convenience: write a frame without an attached fd (Hello / HelloAck
/// / Withdraw / DepositAck / Error).
pub fn send_frame_no_fd(sock: &mut UnixStream, msg: VaultMsg, payload: &[u8]) -> io::Result<()> {
    write_header(sock, msg, payload.len())?;
    if !payload.is_empty() {
        sock.write_all(payload)?;
    }
    Ok(())
}

// ---------------- handshake helpers -----------------------------------

/// Client-side handshake: send Hello, expect HelloAck with matching
/// version.  Call right after `UnixStream::connect`.
pub fn client_handshake(sock: &mut UnixStream) -> io::Result<()> {
    let v = VAULT_PROTO_VERSION.to_le_bytes();
    send_frame_no_fd(sock, VaultMsg::Hello, &v)?;
    match read_frame(sock)? {
        Some((VaultMsg::HelloAck, payload)) => {
            if payload.len() != 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HelloAck wrong payload size",
                ));
            }
            let peer = u32::from_le_bytes(payload[..4].try_into().unwrap());
            if peer != VAULT_PROTO_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("vault peer version {peer} != {VAULT_PROTO_VERSION}"),
                ));
            }
            Ok(())
        }
        Some((VaultMsg::Error, payload)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault server error: {}", String::from_utf8_lossy(&payload)),
        )),
        Some((other, _)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault unexpected frame {other:?} during handshake"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vault closed before HelloAck",
        )),
    }
}

/// Server-side handshake: expect Hello with matching version, reply
/// HelloAck.  Returns the client version on success.
pub fn server_handshake(sock: &mut UnixStream) -> io::Result<u32> {
    match read_frame(sock)? {
        Some((VaultMsg::Hello, payload)) => {
            if payload.len() != 4 {
                let _ = send_frame_no_fd(
                    sock,
                    VaultMsg::Error,
                    b"Hello wrong payload size",
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Hello wrong payload size",
                ));
            }
            let peer = u32::from_le_bytes(payload[..4].try_into().unwrap());
            if peer != VAULT_PROTO_VERSION {
                let _ = send_frame_no_fd(
                    sock,
                    VaultMsg::Error,
                    format!("proto mismatch {peer} != {VAULT_PROTO_VERSION}").as_bytes(),
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("vault peer version {peer} != {VAULT_PROTO_VERSION}"),
                ));
            }
            let v = VAULT_PROTO_VERSION.to_le_bytes();
            send_frame_no_fd(sock, VaultMsg::HelloAck, &v)?;
            Ok(peer)
        }
        Some((other, _)) => {
            let _ = send_frame_no_fd(
                sock,
                VaultMsg::Error,
                format!("expected Hello, got {other:?}").as_bytes(),
            );
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("first frame was {other:?}, not Hello"),
            ))
        }
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vault peer closed before Hello",
        )),
    }
}

// ---------------- one-shot client convenience ------------------------

/// Open + handshake + Deposit + close.  Used by L3 cold start to hand
/// its PTY master fd up to L1 along with a small metadata blob.
pub fn deposit_fd(
    sock_path: &std::path::Path,
    key: u64,
    metadata: &[u8],
    fd: RawFd,
) -> io::Result<()> {
    let mut sock = UnixStream::connect(sock_path)?;
    client_handshake(&mut sock)?;
    let payload = encode_deposit(key, metadata);
    send_frame_with_fd(&sock, VaultMsg::Deposit, &payload, fd)?;
    match read_frame(&mut sock)? {
        Some((VaultMsg::DepositAck, _)) => Ok(()),
        Some((VaultMsg::Error, payload)) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("vault deposit error: {}", String::from_utf8_lossy(&payload)),
        )),
        Some((other, _)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault unexpected reply to Deposit: {other:?}"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vault closed before DepositAck",
        )),
    }
}

/// Open + handshake + Withdraw + close.  Returns `(fd, metadata)` on
/// success.  Used by a resuming L3 to take the PTY master fd back
/// from L1 after its parent process was killed.
pub fn withdraw_fd(
    sock_path: &std::path::Path,
    key: u64,
) -> io::Result<(OwnedFd, Vec<u8>)> {
    let mut sock = UnixStream::connect(sock_path)?;
    client_handshake(&mut sock)?;
    let payload = encode_withdraw(key);
    send_frame_no_fd(&mut sock, VaultMsg::Withdraw, &payload)?;
    let (msg, payload, fd) = recv_frame_with_fd(&sock)?;
    match (msg, fd) {
        (VaultMsg::Delivered, Some(fd)) => {
            let metadata = decode_delivered(&payload)?;
            Ok((fd, metadata))
        }
        (VaultMsg::Error, _) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("vault withdraw error: {}", String::from_utf8_lossy(&payload)),
        )),
        (VaultMsg::Delivered, None) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Delivered without SCM_RIGHTS fd",
        )),
        (other, _) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("vault unexpected reply to Withdraw: {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_payload_roundtrip() {
        let payload = encode_deposit(42, b"hello-meta");
        let (k, m) = decode_deposit(&payload).unwrap();
        assert_eq!(k, 42);
        assert_eq!(m, b"hello-meta");
    }

    #[test]
    fn withdraw_payload_roundtrip() {
        let payload = encode_withdraw(0xDEAD_BEEF_CAFE_0000);
        assert_eq!(decode_withdraw(&payload).unwrap(), 0xDEAD_BEEF_CAFE_0000);
    }

    #[test]
    fn delivered_payload_roundtrip() {
        let payload = encode_delivered(b"metadata-blob");
        assert_eq!(decode_delivered(&payload).unwrap(), b"metadata-blob");
    }

    #[test]
    fn deposit_payload_rejects_short() {
        assert!(decode_deposit(&[0u8; 11]).is_err());
    }

    #[test]
    fn deposit_payload_rejects_mlen_mismatch() {
        let mut p = encode_deposit(7, b"abc");
        // bump declared metadata_len higher than actual
        p[8..12].copy_from_slice(&999u32.to_le_bytes());
        assert!(decode_deposit(&p).is_err());
    }
}
