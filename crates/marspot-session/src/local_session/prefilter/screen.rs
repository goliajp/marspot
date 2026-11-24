//! Which screen the bytes draw on: text drawn on the alternate screen
//! is gone once it is left, so it is no sign of new text on the main
//! one's rows.

/// The private modes that switch to the alternate screen and back.
const ALT_MODES: [u16; 3] = [47, 1047, 1049];

/// A private mode sequence (`CSI ?`) being read.
#[derive(Default)]
pub(super) struct Private {
    param: u16,
    alt: bool,
}

impl Private {
    /// Take a parameter or intermediate byte.
    #[inline]
    pub(super) fn byte(&mut self, b: u8) {
        match b {
            b'0'..=b'9' => self.param = self.param.saturating_mul(10).saturating_add(u16::from(b - b'0')),
            b';' => self.end_param(),
            _ => {}
        }
    }

    fn end_param(&mut self) {
        self.alt |= ALT_MODES.contains(&self.param);
        self.param = 0;
    }

    /// The final byte: whether the sequence leaves the bytes on the
    /// alternate screen, or none when it does not switch.
    pub(super) fn end(&mut self, b: u8) -> Option<bool> {
        self.end_param();
        let alt = std::mem::take(&mut self.alt);
        match b {
            b'h' if alt => Some(true),
            b'l' if alt => Some(false),
            _ => None,
        }
    }
}

/// Where bytes that are not read leave the screen: the last switch in
/// them, if any.  One cut by the chunk's end is not seen; the next ask
/// says where the grid is.
pub(super) fn last_switch(bytes: &[u8]) -> Option<bool> {
    if !super::super::has_byte(bytes, b'?') {
        return None;
    }
    let mut on = None;
    let mut rest = bytes;
    while let Some(at) = rest.windows(3).position(|w| w == b"\x1b[?") {
        rest = &rest[at + 3..];
        let mut p = Private::default();
        let end = rest.iter().position(|&b| !(0x20..0x40).contains(&b)).unwrap_or(rest.len());
        rest[..end].iter().for_each(|&b| p.byte(b));
        if let Some(&f) = rest.get(end) {
            on = p.end(f).or(on);
        }
    }
    on
}
