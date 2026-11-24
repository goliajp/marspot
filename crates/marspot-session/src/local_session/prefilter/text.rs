//! One text looked for, matched a byte at a time.

pub(super) struct Text {
    /// Whose it is, as it was set: a bit of `seen`.
    pub(super) id: usize,
    pub(super) bytes: Vec<u8>,
    back: Vec<u16>,
    pub(super) state: usize,
}

impl Text {
    pub(super) fn new(id: usize, bytes: &[u8]) -> Self {
        let mut back = vec![0u16; bytes.len()];
        let mut k = 0;
        for i in 1..bytes.len() {
            while k > 0 && bytes[i] != bytes[k] {
                k = usize::from(back[k - 1]);
            }
            if bytes[i] == bytes[k] {
                k += 1;
            }
            back[i] = k as u16;
        }
        Self { id, bytes: bytes.to_vec(), back, state: 0 }
    }

    #[inline]
    pub(super) fn step(&mut self, b: u8) -> bool {
        let mut s = self.state;
        while s > 0 && self.bytes[s] != b {
            s = usize::from(self.back[s - 1]);
        }
        if self.bytes[s] == b {
            s += 1;
        }
        if s == self.bytes.len() {
            self.state = usize::from(self.back[s - 1]);
            return true;
        }
        self.state = s;
        false
    }

    pub(super) fn last(&self) -> u8 {
        self.bytes[self.bytes.len() - 1]
    }
}
