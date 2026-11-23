//! Operating system commands: what `ESC ] … ST` asks for.

use super::{Handler, Terminal};
use crate::lx_debug;

/// The most a program may put on the clipboard in one OSC 52.
///
/// 64 KiB is more than any yank and far less than a pty can be made
/// to carry.  A payload over it is dropped whole rather than
/// truncated: half of what was copied is worse than none of it,
/// because nothing tells the user which half.
const OSC52_MAX: usize = 64 * 1024;

/// Command ends (`OSC 133 ; D`) kept by number, so that a reader
/// between two feeds can ask for the first one after the last it saw:
/// a shell's own hook can print a second end, with its own status,
/// right behind the one a command printed.
const ENDS_KEPT: usize = 8;

#[derive(Default)]
pub(super) struct CommandEnds {
    count: u64,
    statuses: [Option<u8>; ENDS_KEPT],
    /// How many `OSC 133 ; B` (the prompt is drawn, input starts).
    inputs: u64,
}

impl CommandEnds {
    fn push(&mut self, status: Option<u8>) {
        self.statuses[(self.count % ENDS_KEPT as u64) as usize] = status;
        self.count += 1;
    }

    pub(super) fn latest(&self) -> (u64, Option<u8>) {
        (self.count, self.status(self.count).flatten())
    }

    pub(super) fn inputs(&self) -> u64 {
        self.inputs
    }

    /// The status of end number `n` (from 1), if it is still kept.
    pub(super) fn status(&self, n: u64) -> Option<Option<u8>> {
        let kept = n >= 1 && n <= self.count && self.count - n < ENDS_KEPT as u64;
        kept.then(|| self.statuses[((n - 1) % ENDS_KEPT as u64) as usize])
    }
}

impl Terminal {
    /// How many `OSC 133 ; D` the terminal has parsed, and what the
    /// latest one said the exit status was.
    pub fn command_end(&self) -> (u64, Option<u8>) {
        self.command_end.latest()
    }

    /// The status of the `n`-th `OSC 133 ; D` (from 1), while it is one
    /// of the latest eight.
    pub fn command_end_status(&self, n: u64) -> Option<Option<u8>> {
        self.command_end.status(n)
    }

    /// How many `OSC 133 ; B` (input starts) the terminal has parsed.
    pub fn input_starts(&self) -> u64 {
        self.command_end.inputs()
    }
}

/// The OSC 8 link the pen is writing, if one is open.
///
/// The id is into the current grid's pool; the URI is kept beside it so
/// the link can be found again in another pool -- the alternate screen
/// is another grid -- and after a sweep has renumbered this one.
#[derive(Default)]
pub(crate) struct Osc8 {
    pub(crate) id: u16,
    pub(crate) uri: String,
}

impl Handler<'_> {
    pub(super) fn osc(&mut self, data: &[u8]) {
        self.flush_u_buf();
        self.flush_cluster_for_break();
        // F3+3.6 — OSC 7 push-based cwd reporting removed; the
        // LayoutModal now pull-fetches cwd via `proc_pidinfo` only
        // when the user opens it (one syscall per pane, not on the
        // hot path).
        let (num, rest) = match data.iter().position(|&b| b == b';') {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &data[data.len()..]),
        };
        let num = std::str::from_utf8(num).ok().and_then(|s| s.parse::<u16>().ok());
        match num {
            // 0 = icon name + window title, 2 = window title.  Stored,
            // not displayed: a pane's label in marspot is DERIVED (its
            // directory), deliberately, so that there is no second
            // source of truth about which pane is which.  Wiring this
            // into the title strip is a product decision, not a
            // protocol one — but a program that says what it is
            // shouldn't have that thrown away in the parser.
            Some(0) | Some(2) => {
                if let Ok(t) = std::str::from_utf8(rest)
                    && self.osc_title != t
                {
                    self.osc_title.clear();
                    // Bounded: a title is a label, and a program that
                    // sends a megabyte of one is not going to get a
                    // megabyte of storage for it.
                    self.osc_title.extend(t.chars().take(256));
                }
            }
            // 10 = default foreground, 11 = default background, and
            // `?` as the value makes it a QUERY.  An unanswered query
            // is how a TUI ends up guessing whether it is on a dark or
            // light terminal — the same shape as the DA1 stall that
            // produced blank rows and misaligned chrome.  Answer in
            // xterm's own form, and echo the OSC number back so a
            // batched query is unambiguous.
            Some(n @ (10 | 11)) if rest.starts_with(b"?") => {
                let p = crate::palette::current();
                let c = if n == 10 { p.fg } else { p.bg };
                let reply = format!("\x1b]{n};{}\x07", crate::palette::xterm_rgb(c));
                self.pending_response.extend_from_slice(reply.as_bytes());
                self.record_response("OSC-COLOR");
            }
            // OSC 133 — where one command's territory ends and the
            // next begins, from the shell's own prompt hooks.
            //
            // The letters are FinalTerm's and the set is not small (a
            // full implementation answers L, A, N, P, B, I, C and D).
            // Four of them carry the boundaries anything would want to
            // scroll to, and the rest are refinements of those; an
            // unknown letter is ignored rather than guessed at.
            //
            // Options arrive as `;key=value` after the letter and are
            // skipped, except `D`'s first parameter, which is the exit
            // status. "No status" is a different thing from zero: a
            // shell that did not say cannot be reported as success.
            Some(133) => {
                let body = std::str::from_utf8(rest).unwrap_or("");
                let mut parts = body.split(';');
                let mark = match parts.next() {
                    Some("A") | Some("P") => crate::grid::PromptMark::PromptStart,
                    Some("B") => {
                        self.command_end.inputs += 1;
                        crate::grid::PromptMark::InputStart
                    }
                    Some("C") => crate::grid::PromptMark::OutputStart,
                    Some("D") => {
                        let status = parts
                            .next()
                            .filter(|p| !p.is_empty() && !p.contains('='))
                            .and_then(|p| p.parse::<u8>().ok());
                        self.command_end.push(status);
                        crate::grid::PromptMark::CommandEnd(status)
                    }
                    _ => return,
                };
                let (_, row) = self.grid.cursor();
                self.grid.set_row_prompt(row, mark);
            }
            // OSC 8 — the cells written from here on belong to a link.
            Some(8) => self.osc8(data.len(), rest),
            // OSC 52 — a program putting something on the clipboard.
            //
            // `52 ; <targets> ; <base64>`.  This is how yanking in vim
            // over ssh reaches the Mac's pasteboard, and it is the one
            // thing a terminal can do for a remote editor that nothing
            // else can.
            //
            // The read form (`52 ; c ; ?`) is refused, deliberately and
            // silently.  Answering it hands any program that can write
            // to this pty whatever the user last copied — a password,
            // a token — and no program needs that to do its job.  A
            // terminal that stays quiet is read as one without the
            // feature, which is the right amount of information.
            Some(52) => {
                let payload = match rest.iter().position(|&b| b == b';') {
                    Some(i) => &rest[i + 1..],
                    None => &rest[rest.len()..],
                };
                if payload != b"?"
                    && let Some(bytes) = crate::base64::decode(payload, OSC52_MAX)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    *self.osc_clipboard = Some(text);
                }
            }
            // OSC 4 — one or more palette entries: `4 ; n ; ? ; n ; ?`
            // asks, `4 ; n ; rgb:…` sets.  vim and tmux read these to
            // decide what they can draw with, and a terminal that stays
            // quiet is read as one with no palette to speak of.  Answered
            // per pair, in the same order asked, so a batched query is
            // unambiguous.  A set entry stands over the theme's until
            // OSC 104 or a reset takes it back.
            Some(4) => {
                let mut reply = String::new();
                let mut fields = rest.split(|&b| b == b';');
                while let (Some(idx), Some(val)) = (fields.next(), fields.next()) {
                    let Some(i) = std::str::from_utf8(idx).ok().and_then(|s| s.parse::<u8>().ok())
                    else {
                        continue;
                    };
                    if val != b"?" {
                        if let Some(rgb) = super::modes::parse_color_spec(val) {
                            self.modes.palette.set(i, rgb);
                        }
                        continue;
                    }
                    let c = match self.modes.palette.get(i) {
                        Some(rgb) => crate::palette::unit(rgb),
                        None => crate::palette::current().indexed(i),
                    };
                    reply.push_str(&format!("\x1b]4;{i};{}\x07", crate::palette::xterm_rgb(c)));
                }
                if !reply.is_empty() {
                    self.pending_response.extend_from_slice(reply.as_bytes());
                    self.record_response("OSC-PALETTE");
                }
            }
            // OSC 104 — back to the theme's colour: the entries named,
            // or every entry when none is.
            Some(104) => {
                if rest.is_empty() {
                    self.modes.palette.reset_all();
                }
                for i in rest.split(|&b| b == b';').filter_map(|f| std::str::from_utf8(f).ok()?.parse::<u8>().ok()) {
                    self.modes.palette.reset(i);
                }
            }
            // OSC 12 — the cursor's colour.  Same shape as 10 and 11,
            // and asked by the same programs for the same reason.
            Some(12) if rest.starts_with(b"?") => {
                let c = crate::palette::unit(crate::palette::current().cursor);
                let reply = format!("\x1b]12;{}\x07", crate::palette::xterm_rgb(c));
                self.pending_response.extend_from_slice(reply.as_bytes());
                self.record_response("OSC-COLOR");
            }
            _ => {
                lx_debug!(
                    "term.osc.unsupported",
                    "OSC payload with no handler",
                    bytes = data.len(),
                    num = num.unwrap_or(u16::MAX) as u64
                );
            }
        }
    }

    /// `8 ; <params> ; <URI>`.  An empty URI closes the open link.  The
    /// parameters are `key=value` pairs; the only one defined, `id=`,
    /// says which pieces of a link split across lines belong together,
    /// which the pool already knows by URI, so all are skipped -- the
    /// spec says unknown ones must be.
    ///
    /// A URI that is too long, cut short by the parser's bound, or not
    /// in a scheme a link may have closes the open link and opens none:
    /// the text that follows is shown, and is not anybody's link.
    fn osc8(&mut self, osc_len: usize, rest: &[u8]) {
        let uri = match rest.iter().position(|&b| b == b';') {
            Some(i) => &rest[i + 1..],
            None => &rest[rest.len()..],
        };
        self.close_link();
        if uri.is_empty() {
            return;
        }
        let Ok(uri) = std::str::from_utf8(uri) else { return };
        if uri.len() > crate::osc8::URI_MAX || osc_len >= crate::parser::MAX_OSC_LEN {
            crate::lx_info!("term.osc8.too_long", "OSC 8 URI over the bound, ignored", bytes = osc_len);
            return;
        }
        if !crate::osc8::uri_allowed(uri) {
            lx_debug!("term.osc8.refused", "OSC 8 URI not in an allowed scheme", bytes = uri.len());
            return;
        }
        self.osc8.uri.push_str(uri);
        self.relink();
    }

    /// Give the open link its id in the current grid's pool: after the
    /// pen moved to another grid, or before a new link is numbered.
    pub(super) fn relink(&mut self) {
        if self.osc8.uri.is_empty() {
            self.close_link();
            return;
        }
        if self.grid.link_sweep_due() {
            self.grid.sweep_links();
        }
        // a full pool gives none: the text is written, unlinked
        self.osc8.id = self.grid.link_id_for(&self.osc8.uri).unwrap_or(0);
        self.link = self.osc8.id;
    }

    /// End the open link, if any.
    pub(super) fn close_link(&mut self) {
        *self.osc8 = Osc8::default();
        self.link = 0;
    }
}
