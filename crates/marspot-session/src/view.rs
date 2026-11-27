//! Where in its history the core asks the pane's view to be: a scroll
//! to an offset, or a jump to the prompt before or after the view.

use marspot_term::lx_event;
use marspot_term::shell_proto::{Frame, MsgType, PromptDir, encode_prompt_jumped};

use crate::SessionImpl;
use crate::control_writer::ControlWriter;

/// L2 forwarded a view offset.  With mouse tracking on, L2 encodes the
/// wheel for the program instead, so this is the scrollback's path.
pub(crate) fn scroll(session: &SessionImpl, off: u16, pending: &mut Option<u16>) {
    lx_event!("L3_SCROLL_RECV", "GridScroll received from L2", session_id = session.id(), off = off as u32);
    *pending = Some(off);
}

/// Move the view to the prompt `dir` of the line at its top, where the
/// view is `pending` if a scroll is still to be applied this turn, else
/// `current`; and tell the core where it went.  No prompt that way, or
/// a program on the alternate screen, which has no history of its own:
/// the view stays and the core is told nothing.
pub(crate) fn prompt_jump(
    session: &SessionImpl,
    dir: PromptDir,
    pending: &mut Option<u16>,
    current: u16,
    poke: Option<&mut ControlWriter>,
) {
    let term = session.terminal();
    if term.in_alt_screen() {
        return;
    }
    let grid = term.grid();
    let from = pending.unwrap_or(current);
    let to = match dir {
        PromptDir::Back => grid.prompt_above(from),
        PromptDir::Forward => grid.prompt_below(from).map(|(off, _)| off),
    };
    lx_event!(
        "L3_PROMPT_JUMP",
        "a jump to a prompt was asked for",
        session_id = session.id(),
        back = (dir == PromptDir::Back) as u32,
        from = from as u32,
        found = to.is_some() as u32,
        to = to.unwrap_or(0) as u32
    );
    let Some(to) = to else { return };
    *pending = Some(to);
    if let Some(poke) = poke {
        poke.send(Frame::new(MsgType::PromptJumped, encode_prompt_jumped(to)));
    }
}
