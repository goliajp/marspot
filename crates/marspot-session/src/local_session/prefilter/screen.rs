//! Which screen the bytes draw on: text drawn on the alternate screen
//! is gone once it is left, so it is no sign of new text on the main
//! one's rows.

/// The private modes that switch to the alternate screen and back.
const ALT_MODES: [u16; 3] = [47, 1047, 1049];
/// Parameters the parser keeps; it drops the ones after.
const PARAMS: u8 = 16;

/// A private mode sequence (`CSI ?`) being read.
#[derive(Default)]
pub(super) struct Private {
    param: u16,
    /// Parameters ended so far.
    count: u8,
    alt: bool,
    /// The terminal sets no mode for it: the parser ignores it, or it
    /// has intermediates besides the `?`.
    ignored: bool,
}

impl Private {
    /// Take a byte before the final one; a control is executed by the
    /// parser and leaves the sequence going on.
    #[inline]
    pub(super) fn byte(&mut self, b: u8) {
        match b {
            b'0'..=b'9' => self.param = self.param.saturating_mul(10).saturating_add(u16::from(b - b'0')),
            // the terminal sets every parameter, one after a colon too
            b';' | b':' => self.end_param(),
            0x20..=0x2f | 0x3c..=0x3f | 0x80.. => self.ignored = true,
            _ => {}
        }
    }

    fn end_param(&mut self) {
        self.alt |= self.count < PARAMS && ALT_MODES.contains(&self.param);
        self.count = self.count.saturating_add(1);
        self.param = 0;
    }

    /// The final byte: whether the sequence leaves the bytes on the
    /// alternate screen, or none when it does not switch.
    pub(super) fn end(&mut self, b: u8) -> Option<bool> {
        self.end_param();
        let done = std::mem::take(self);
        let alt = done.alt && !done.ignored;
        match b {
            b'h' if alt => Some(true),
            b'l' if alt => Some(false),
            _ => None,
        }
    }
}

/// Where bytes that are not read leave the screen: the last switch in
/// them, a full reset included, if any; and where their last escape
/// is.  A switch cut by the chunk's end is not seen; the next ask says
/// where the grid is.
pub(super) fn scan(bytes: &[u8]) -> (Option<bool>, Option<usize>) {
    let mut on = None;
    let mut last = None;
    let mut at = 0;
    while let Some(esc) = super::super::find_byte(&bytes[at..], 0x1b) {
        last = Some(at + esc);
        at += esc + 1;
        match &bytes[at..] {
            [b'c', ..] => on = Some(false),
            [b'[', b'?', rest @ ..] => {
                // as in the parser: up to a final byte, or the escape,
                // CAN or SUB that ends the sequence
                let end = rest.iter().position(|&b| super::is_abort(b) || (0x40..=0x7e).contains(&b)).unwrap_or(rest.len());
                let mut p = Private::default();
                rest[..end].iter().for_each(|&b| p.byte(b));
                if let Some(&f) = rest.get(end).filter(|&&f| !super::is_abort(f)) {
                    on = p.end(f).or(on);
                }
                // an escape that ends it is the next one looked at
                at += 2 + end;
            }
            _ => {}
        }
    }
    (on, last)
}

pub(super) fn last_switch(bytes: &[u8]) -> Option<bool> {
    scan(bytes).0
}
