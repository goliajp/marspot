//! A WebSocket client over a unix socket — the transport of the Codex
//! app-server control socket (`<CODEX_HOME>/app-server-control/
//! app-server-control.sock`).  Client side only: text messages out,
//! text messages in, pings answered, fragments reassembled.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sha1::{base64, sha1};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Largest message accepted.  The app-server's biggest answers are
/// thread histories, which callers avoid asking for (`excludeTurns`);
/// anything past this is a protocol fault, not a message to buffer.
pub const MAX_MESSAGE: usize = 32 << 20;
const MAX_HANDSHAKE: usize = 16 << 10;

pub struct Ws {
    r: WsReader,
    w: WsWriter,
}

/// The receiving half.  Answers pings itself, through the shared
/// writer, so a reader on its own thread keeps the connection alive.
pub struct WsReader {
    s: UnixStream,
    buf: Vec<u8>,
    partial: Option<Vec<u8>>,
    w: WsWriter,
}

/// The sending half.  Cloneable; whole frames are written under one
/// lock, so frames from two threads never interleave on the wire.
#[derive(Clone)]
pub struct WsWriter {
    s: Arc<Mutex<UnixStream>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Text(String),
    /// The server closed the connection.
    Closed,
}

fn proto(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

impl Ws {
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Ws> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        let mut nonce = [0u8; 16];
        crate::rand::fill(&mut nonce);
        let key = base64(&nonce);
        let w = WsWriter { s: Arc::new(Mutex::new(s.try_clone()?)) };
        let mut ws = Ws { r: WsReader { s, buf: Vec::new(), partial: None, w: w.clone() }, w };
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.w.write_raw(req.as_bytes())?;
        let head = ws.r.read_head()?;
        let mut lines = head.split("\r\n");
        if !lines.next().is_some_and(|l| l.starts_with("HTTP/1.1 101")) {
            return Err(proto("handshake refused"));
        }
        let want = base64(&sha1(format!("{key}{GUID}").as_bytes()));
        let accepted = lines
            .filter_map(|l| l.split_once(':'))
            .any(|(k, v)| k.trim().eq_ignore_ascii_case("sec-websocket-accept") && v.trim() == want);
        if !accepted {
            return Err(proto("handshake accept mismatch"));
        }
        Ok(ws)
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.r.s.set_read_timeout(d)
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.w.send_text(text)
    }

    pub fn recv(&mut self) -> io::Result<Msg> {
        self.r.recv()
    }

    pub fn into_split(self) -> (WsReader, WsWriter) {
        (self.r, self.w)
    }
}

impl WsWriter {
    pub fn send_text(&self, text: &str) -> io::Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    fn write_raw(&self, bytes: &[u8]) -> io::Result<()> {
        self.s.lock().unwrap_or_else(|p| p.into_inner()).write_all(bytes)
    }

    /// Client frames are always masked (RFC 6455 5.3).
    fn send_frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push(0x80 | opcode);
        let n = payload.len();
        if n < 126 {
            out.push(0x80 | n as u8);
        } else if n <= 0xFFFF {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        crate::rand::fill(&mut mask);
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.write_raw(&out)
    }

    /// Stop both halves: a reader blocked on this connection returns.
    pub fn shutdown(&self) {
        let _ = self.s.lock().unwrap_or_else(|p| p.into_inner()).shutdown(std::net::Shutdown::Both);
    }
}

impl WsReader {
    fn read_head(&mut self) -> io::Result<String> {
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                self.buf.drain(..end + 4);
                return Ok(head);
            }
            if self.buf.len() > MAX_HANDSHAKE {
                return Err(proto("handshake too long"));
            }
            self.fill()?;
        }
    }

    /// None blocks; otherwise a read that waits longer fails with
    /// `WouldBlock`, and a partial message stays buffered for the next
    /// call.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.s.set_read_timeout(d)
    }

    fn fill(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 64 << 10];
        let n = self.s.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// One frame from the buffer, if a whole one is there:
    /// (fin, opcode, payload).
    fn take_frame(&mut self) -> io::Result<Option<(bool, u8, Vec<u8>)>> {
        let b = &self.buf;
        if b.len() < 2 {
            return Ok(None);
        }
        let (fin, opcode, masked) = (b[0] & 0x80 != 0, b[0] & 0x0F, b[1] & 0x80 != 0);
        let (len, mut off) = match b[1] & 0x7F {
            126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
            127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize, 10),
            126 | 127 => return Ok(None),
            n => (n as usize, 2),
        };
        if len > MAX_MESSAGE {
            return Err(proto("frame too large"));
        }
        let mask = if masked {
            if b.len() < off + 4 {
                return Ok(None);
            }
            off += 4;
            Some([b[off - 4], b[off - 3], b[off - 2], b[off - 1]])
        } else {
            None
        };
        if b.len() < off + len {
            return Ok(None);
        }
        let mut payload = b[off..off + len].to_vec();
        if let Some(m) = mask {
            payload.iter_mut().enumerate().for_each(|(i, x)| *x ^= m[i % 4]);
        }
        self.buf.drain(..off + len);
        Ok(Some((fin, opcode, payload)))
    }

    /// The next whole text message.  A fragment read before a timeout
    /// is kept in `partial`, so the next call carries on where this one
    /// stopped instead of returning the tail as a message of its own.
    pub fn recv(&mut self) -> io::Result<Msg> {
        loop {
            let Some((fin, opcode, payload)) = self.take_frame()? else {
                self.fill()?;
                continue;
            };
            match opcode {
                0x9 => self.w.send_frame(0xA, &payload)?,
                0xA => {}
                0x8 => {
                    let _ = self.w.send_frame(0x8, &payload);
                    return Ok(Msg::Closed);
                }
                0x1 | 0x0 => {
                    let acc = self.partial.get_or_insert_with(Vec::new);
                    acc.extend_from_slice(&payload);
                    if acc.len() > MAX_MESSAGE {
                        return Err(proto("message too large"));
                    }
                    if fin {
                        let bytes = self.partial.take().unwrap_or_default();
                        return String::from_utf8(bytes).map(Msg::Text).map_err(|_| proto("text is not UTF-8"));
                    }
                }
                _ => return Err(proto("unexpected opcode")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A server side written from RFC 6455 directly, so the client is
    /// checked against the spec rather than against itself.
    fn serve(listener: UnixListener, frames: Vec<Vec<u8>>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                c.read_exact(&mut byte).unwrap();
                req.push(byte[0]);
            }
            let req = String::from_utf8(req).unwrap();
            let key = req.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().trim().to_string();
            let accept = base64(&sha1(format!("{key}{GUID}").as_bytes()));
            write!(c, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
            for f in frames {
                c.write_all(&f).unwrap();
            }
            let mut rest = Vec::new();
            c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let _ = c.read_to_end(&mut rest);
            rest
        })
    }

    fn sock(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mp-ws-{tag}-{}.sock", crate::rand::hex(6)))
    }

    #[test]
    fn reassembles_fragments_and_answers_pings() {
        let path = sock("frag");
        let l = UnixListener::bind(&path).unwrap();
        let mut long = vec![0x81, 126];
        long.extend_from_slice(&300u16.to_be_bytes());
        long.extend(std::iter::repeat_n(b'x', 300));
        let frames = vec![
            vec![0x01, 3, b'a', b'b', b'c'], // text, not final
            vec![0x89, 2, b'p', b'!'],       // ping in between
            vec![0x80, 2, b'd', b'e'],       // continuation, final
            long,
        ];
        let h = serve(l, frames);
        let mut ws = Ws::connect(&path, Duration::from_secs(5)).unwrap();
        assert_eq!(ws.recv().unwrap(), Msg::Text("abcde".into()));
        assert_eq!(ws.recv().unwrap(), Msg::Text("x".repeat(300)));
        ws.send_text("hi").unwrap();
        drop(ws);
        let rest = h.join().unwrap();
        // pong first (masked, opcode 0xA), then the masked text frame
        assert_eq!(rest[0], 0x8A);
        let pong_mask = &rest[2..6];
        assert_eq!([rest[6] ^ pong_mask[0], rest[7] ^ pong_mask[1]], *b"p!");
        let t = &rest[8..];
        assert_eq!(t[0], 0x81);
        assert_eq!(t[1], 0x80 | 2);
        assert_eq!([t[6] ^ t[2], t[7] ^ t[3]], *b"hi");
        let _ = std::fs::remove_file(&path);
    }

    /// A timeout between two fragments must not lose the first one.
    #[test]
    fn a_fragment_survives_a_read_timeout() {
        let path = sock("slow");
        let l = UnixListener::bind(&path).unwrap();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                c.read_exact(&mut byte).unwrap();
                req.push(byte[0]);
            }
            let req = String::from_utf8(req).unwrap();
            let key = req.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().trim().to_string();
            let accept = base64(&sha1(format!("{key}{GUID}").as_bytes()));
            write!(c, "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
            c.write_all(&[0x01, 2, b'a', b'b']).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            c.write_all(&[0x80, 2, b'c', b'd']).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let mut ws = Ws::connect(&path, Duration::from_secs(5)).unwrap();
        ws.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let err = ws.recv().unwrap_err();
        assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{err}");
        ws.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(ws.recv().unwrap(), Msg::Text("abcd".into()));
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn close_frame_reads_as_closed() {
        let path = sock("close");
        let l = UnixListener::bind(&path).unwrap();
        let h = serve(l, vec![vec![0x88, 0]]);
        let mut ws = Ws::connect(&path, Duration::from_secs(5)).unwrap();
        assert_eq!(ws.recv().unwrap(), Msg::Closed);
        drop(ws);
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_wrong_accept_is_refused() {
        let path = sock("bad");
        let l = UnixListener::bind(&path).unwrap();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 512];
            let _ = c.read(&mut b);
            c.write_all(b"HTTP/1.1 101 OK\r\nSec-WebSocket-Accept: nope\r\n\r\n").unwrap();
        });
        assert!(Ws::connect(&path, Duration::from_secs(5)).is_err());
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
