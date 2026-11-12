//! Whole-slice entry points: the same transitions `advance` makes,
//! taken for a run of bytes at once.

use super::{Parser, ParserCallbacks, State};

impl Parser {
    /// Consume the head of a control sequence from `bytes`, which must
    /// start with `ESC [` while the parser is in plain Ground.
    ///
    /// Takes the introducer and then digits and `;` for as long as
    /// they last.  A final byte right after them dispatches.  Anything
    /// else is left unconsumed with the parser in the state `advance`
    /// would have reached, so the per-byte path carries on from there:
    /// private markers, colons, intermediates, controls inside the
    /// sequence and a sequence cut by the end of the slice all go that
    /// way.  Returns the bytes consumed, at least two.
    pub fn csi_slice<C: ParserCallbacks>(&mut self, cb: &mut C, bytes: &[u8]) -> usize {
        debug_assert!(self.in_ground_plain() && bytes.starts_with(b"\x1b["));
        self.reset_seq();
        self.state = State::CsiEntry;
        let mut i = 2;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'0'..=b'9' => {
                    if self.state == State::CsiEntry {
                        self.start_param_digit(b);
                        self.state = State::CsiParam;
                    } else {
                        self.continue_param_digit(b);
                    }
                }
                b';' => {
                    self.commit_param();
                    self.state = State::CsiParam;
                }
                0x40..=0x7E => {
                    self.commit_param_if_pending();
                    cb.csi_dispatch(
                        self.params_slice(),
                        self.subs_slice(),
                        self.intermediates_slice(),
                        b,
                    );
                    self.state = State::Ground;
                    return i + 1;
                }
                _ => break,
            }
            i += 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Parser, ParserCallbacks};

    #[derive(Default, PartialEq, Debug)]
    struct Log(Vec<String>);
    impl ParserCallbacks for Log {
        fn print(&mut self, ch: char) {
            self.0.push(format!("p{ch:?}"));
        }
        fn execute(&mut self, byte: u8) {
            self.0.push(format!("x{byte}"));
        }
        fn esc_dispatch(&mut self, i: &[u8], byte: u8) {
            self.0.push(format!("e{i:?}{byte}"));
        }
        fn csi_dispatch(&mut self, p: &[u16], s: &[bool], i: &[u8], byte: u8) {
            self.0.push(format!("c{p:?}{s:?}{i:?}{byte}"));
        }
        fn osc_dispatch(&mut self, data: &[u8]) {
            self.0.push(format!("o{data:?}"));
        }
    }

    /// feed `seq` once a byte at a time and once through the slice
    /// entry (falling back to `advance` where it stops), cut at `cut`
    fn both(seq: &[u8], cut: usize) -> (Log, Log) {
        let mut a = Log::default();
        let mut pa = Parser::new();
        for &b in seq {
            pa.advance(&mut a, b);
        }
        let mut b = Log::default();
        let mut pb = Parser::new();
        for part in [&seq[..cut], &seq[cut..]] {
            let mut i = 0;
            while i < part.len() {
                if pb.in_ground_plain() && part[i..].starts_with(b"\x1b[") {
                    i += pb.csi_slice(&mut b, &part[i..]);
                } else {
                    pb.advance(&mut b, part[i]);
                    i += 1;
                }
            }
        }
        (a, b)
    }

    #[test]
    fn a_sequence_taken_as_a_slice_dispatches_what_the_bytes_would() {
        let mut cases: Vec<Vec<u8>> = vec![
            b"\x1b[m".to_vec(),
            b"\x1b[0m".to_vec(),
            b"\x1b[;m".to_vec(),
            b"\x1b[1;m".to_vec(),
            b"\x1b[;;5m".to_vec(),
            b"\x1b[38;5;196m".to_vec(),
            b"\x1b[38;2;255;128;0;48;2;1;2;3m".to_vec(),
            b"\x1b[4:3m".to_vec(),
            b"\x1b[38:2::255:0:0m".to_vec(),
            b"\x1b[?25h".to_vec(),
            b"\x1b[>4;1m".to_vec(),
            b"\x1b[1 q".to_vec(),
            b"\x1b[12;40H".to_vec(),
            b"\x1b[99999999999m".to_vec(),
            b"\x1b[65535;65536m".to_vec(),
            b"\x1b[1\n2m".to_vec(),
            b"\x1b[1\x1b[2m".to_vec(),
            b"\x1b[1\x18m".to_vec(),
            b"\x1b[1\x7f2m".to_vec(),
            b"\x1b[3<m".to_vec(),
            b"\x1b[1\xe4\xb8\xadm".to_vec(),
            b"\x1b[".to_vec(),
            b"\x1b[12".to_vec(),
        ];
        // 0..=32 parameters, past the sixteen the parser keeps
        for n in 0..=32 {
            let mut s = b"\x1b[".to_vec();
            for k in 0..n {
                if k > 0 {
                    s.push(b';');
                }
                s.extend_from_slice((30 + k).to_string().as_bytes());
            }
            s.push(b'm');
            cases.push(s);
        }
        for seq in &cases {
            let mut seq = seq.clone();
            seq.extend_from_slice(b"ok");
            for cut in 0..=seq.len() {
                let (a, b) = both(&seq, cut);
                assert_eq!(a, b, "{:?} cut at {cut}", String::from_utf8_lossy(&seq));
            }
        }
    }
}
