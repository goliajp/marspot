//! The terminal flags a repair deals in.

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Flags {
    pub(super) input: libc::tcflag_t,
    pub(super) output: libc::tcflag_t,
    pub(super) local: libc::tcflag_t,
}

pub(super) const OUTPUT: Flags = Flags { input: 0, output: libc::OPOST | libc::ONLCR, local: 0 };
pub(super) const ALL: Flags = Flags {
    input: libc::ICRNL | libc::BRKINT,
    output: libc::OPOST | libc::ONLCR,
    local: libc::ISIG | libc::IEXTEN,
};

impl Flags {
    pub(super) fn of(t: &libc::termios) -> Self {
        Self { input: t.c_iflag, output: t.c_oflag, local: t.c_lflag }
    }

    pub(super) fn and(self, o: Self) -> Self {
        Self { input: self.input & o.input, output: self.output & o.output, local: self.local & o.local }
    }

    /// Those of `self` that `now` does not have.
    pub(super) fn missing_from(self, now: Self) -> Self {
        Self { input: self.input & !now.input, output: self.output & !now.output, local: self.local & !now.local }
    }

    pub(super) fn is_empty(self) -> bool {
        self == Self::default()
    }
}
