//! Command boundaries a shell marks with `OSC 133`.

/// Where a shell says one command's territory ends and the next
/// begins, from `OSC 133`.
///
/// A shell that emits these turns a wall of output into a sequence of
/// commands: where the prompt was, where what you typed began, where
/// its output began, and what it exited with. Scrolling back to "the
/// command before this one" needs nothing more than this, and without
/// it there is nothing to scroll to -- a terminal cannot tell a prompt
/// from any other line that happens to start with a glyph.
///
/// One byte a row, beside the wrap flag, and only for the rows a shell
/// actually marked. It is not written to disk: the scrollback record
/// has one byte for `wrapped` and a reader decides on `!= 0`, so a
/// spare bit in it would read as a wrapped line to a binary that
/// predates this -- the quiet kind of wrong. Persisting it waits for
/// the sidecar in `20261001-line-identity.md`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PromptMark {
    #[default]
    None,
    /// `OSC 133 ; A` -- a prompt starts on this row.
    PromptStart,
    /// `OSC 133 ; B` -- the prompt ended; what follows is what the
    /// person typed.
    InputStart,
    /// `OSC 133 ; C` -- the command was sent; what follows is its
    /// output.
    OutputStart,
    /// `OSC 133 ; D ; <status>` -- the command finished. `None` when
    /// the shell reported no status, which is different from zero.
    CommandEnd(Option<u8>),
}

impl PromptMark {
    /// Two bytes: what kind, then the exit status for the one kind
    /// that carries one.  Fixed width so a file of these is addressed
    /// by line index rather than scanned.
    pub(crate) fn to_bytes(self) -> [u8; 2] {
        match self {
            Self::None => [0, 0],
            Self::PromptStart => [1, 0],
            Self::InputStart => [2, 0],
            Self::OutputStart => [3, 0],
            // "the shell did not say" and "the shell said zero" are
            // different answers and get different encodings.
            Self::CommandEnd(None) => [4, 0],
            Self::CommandEnd(Some(s)) => [5, s],
        }
    }

    /// An encoding this build does not know reads as no mark, which is
    /// the same safe failure a missing sidecar gives.
    pub(crate) fn from_bytes(b: [u8; 2]) -> Self {
        match b[0] {
            1 => Self::PromptStart,
            2 => Self::InputStart,
            3 => Self::OutputStart,
            4 => Self::CommandEnd(None),
            5 => Self::CommandEnd(Some(b[1])),
            _ => Self::None,
        }
    }
}

impl super::Grid {
    /// The view offset that puts the nearest prompt start above the
    /// line at the top of the view at the top of the view, or `None`
    /// when there is no earlier prompt a view can reach.
    ///
    /// `view_offset` counts rows scrolled back from the live screen,
    /// the way `cell_at_view` does.  The marks are read where they are
    /// kept, one line at a time and nothing collected; a view reaches
    /// at most `u16::MAX` lines back, so that is as far as it looks.
    pub fn prompt_above(&self, view_offset: u16) -> Option<u16> {
        let sb = self.scrollback_len();
        let top = sb - (view_offset as usize).min(sb);
        let reach = sb.saturating_sub(u16::MAX as usize);
        (reach..top)
            .rev()
            .find(|&i| self.scrollback_prompt(i) == PromptMark::PromptStart)
            .map(|i| (sb - i) as u16)
    }

    /// The view offset that puts the nearest prompt start below the
    /// line at the top of the view at the top of the view, `0` (the
    /// live screen) when that prompt is on the live screen, or `None`
    /// when there is no later prompt.
    pub fn prompt_below(&self, view_offset: u16) -> Option<u16> {
        let sb = self.scrollback_len();
        let top = sb - (view_offset as usize).min(sb);
        if top >= sb {
            return None;
        }
        if let Some(i) = (top + 1..sb).find(|&i| self.scrollback_prompt(i) == PromptMark::PromptStart) {
            return Some((sb - i) as u16);
        }
        (0..self.rows()).any(|r| self.row_prompt(r) == PromptMark::PromptStart).then_some(0)
    }
}
