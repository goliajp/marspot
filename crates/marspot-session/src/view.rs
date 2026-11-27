//! Where in its history the core asks the pane's view to be: a scroll
//! to an offset, or a jump to the prompt before or after the view.

use marspot_term::lx_event;
use marspot_term::shell_proto::{
    Frame, MsgType, PromptDir, PromptJump, PromptJumped, encode_prompt_jumped, view_ask_seq,
};

use crate::SessionImpl;
use crate::control_writer::ControlWriter;

/// What the core asked of the view in one turn: the offset to publish
/// at, if it moved, and the number of its last ask, which the frames
/// carry from then on.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Asked {
    pub(crate) off: Option<u16>,
    pub(crate) seq: Option<u16>,
}

/// L2 forwarded a view offset.  With mouse tracking on, L2 encodes the
/// wheel for the program instead, so this is the scrollback's path.
/// A core from before the ask numbers sends none.
pub(crate) fn scroll(session: &SessionImpl, off: u16, seq: Option<u16>, pending: &mut Asked) {
    lx_event!("L3_SCROLL_RECV", "GridScroll received from L2", session_id = session.id(), off = off as u32);
    pending.off = Some(off);
    pending.seq = seq.or(pending.seq);
}

/// Move the view to the prompt `jump.dir` of the line at its top, where
/// the view is `pending` if a scroll is still to be applied this turn,
/// else `current`; and tell the core where it went, which row the
/// prompt is on, and which of its jumps this answers.  No prompt that
/// way, or a program on the alternate screen, which has no history of
/// its own: the view stays and the core is told nothing.  Either way
/// the frames say the jump was applied.
pub(crate) fn prompt_jump(
    session: &SessionImpl,
    jump: PromptJump,
    pending: &mut Asked,
    current: u16,
    poke: Option<&mut ControlWriter>,
) {
    pending.seq = Some(view_ask_seq(jump.seq));
    let term = session.terminal();
    if term.in_alt_screen() {
        return;
    }
    let grid = term.grid();
    let from = pending.off.unwrap_or(current);
    let to = match jump.dir {
        PromptDir::Back => grid.prompt_above(from).map(|off| (off, 0)),
        PromptDir::Forward => grid.prompt_below(from),
    };
    lx_event!(
        "L3_PROMPT_JUMP",
        "a jump to a prompt was asked for",
        session_id = session.id(),
        back = (jump.dir == PromptDir::Back) as u32,
        from = from as u32,
        found = to.is_some() as u32,
        to = to.map_or(0, |t| t.0) as u32,
        row = to.map_or(0, |t| t.1) as u32,
        seq = jump.seq
    );
    let Some((view_offset, row)) = to else { return };
    pending.off = Some(view_offset);
    if let Some(poke) = poke {
        let said = PromptJumped { view_offset, row, seq: Some(jump.seq) };
        poke.send(Frame::new(MsgType::PromptJumped, encode_prompt_jumped(said)));
    }
}
