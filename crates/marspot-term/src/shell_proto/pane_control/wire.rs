//! Reading and writing the pieces the pane-control payloads are made of.

use std::io;

use crate::shell_proto::MAX_PAYLOAD_LEN;

pub(super) fn bad(what: &str, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what}: {why}"))
}

pub(super) struct Rd<'a> {
    b: &'a [u8],
    at: usize,
    what: &'static str,
}

impl<'a> Rd<'a> {
    pub(super) fn new(b: &'a [u8], what: &'static str) -> Self {
        Rd { b, at: 0, what }
    }
    pub(super) fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| bad(self.what, "payload truncated"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    pub(super) fn arr<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub(super) fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(super) fn u16(&mut self) -> io::Result<u16> {
        self.arr().map(u16::from_le_bytes)
    }
    pub(super) fn u32(&mut self) -> io::Result<u32> {
        self.arr().map(u32::from_le_bytes)
    }
    pub(super) fn u64(&mut self) -> io::Result<u64> {
        self.arr().map(u64::from_le_bytes)
    }
    pub(super) fn text(&mut self) -> io::Result<String> {
        let n = self.u16()? as usize;
        let raw = self.take(n)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| bad(self.what, "text is not utf-8"))
    }
}

pub(super) fn put_text(v: &mut Vec<u8>, s: &str, what: &str) -> io::Result<()> {
    let n = u16::try_from(s.len()).map_err(|_| bad(what, "text over 65535 bytes"))?;
    v.extend_from_slice(&n.to_le_bytes());
    v.extend_from_slice(s.as_bytes());
    Ok(())
}

pub(super) fn finish(v: Vec<u8>, what: &str) -> io::Result<Vec<u8>> {
    if v.len() > MAX_PAYLOAD_LEN {
        return Err(bad(what, "payload over the frame limit"));
    }
    Ok(v)
}
