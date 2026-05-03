//! tmux control-mode (`tmux -CC`) protocol parser.
//!
//! In control mode, tmux multiplexes one PTY between many panes,
//! emitting line-prefixed notifications and command responses on the
//! same byte stream.  This module is a streaming parser that converts
//! that byte stream into a sequence of [`Event`]s suitable for a UI
//! layer to act on.
//!
//! Deliberately ignorant of mars internals: this parser knows
//! nothing about Session / Renderer / winit.  It takes `&[u8]` in
//! and emits typed events out, making it independently testable
//! and reusable for any other front-end (mcli, a hypothetical
//! TUI control panel, etc.).
//!
//! Reference: `man tmux`, "CONTROL MODE" section.

/// Parsed event from the tmux control stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Pane bytes — the raw VT data for one pane, already
    /// octal-unescaped from the wire format.  Feed this directly
    /// into a `Terminal` for that pane.
    Output { pane_id: u32, bytes: Vec<u8> },
    /// A new window appeared in the current session.
    WindowAdd { window_id: u32 },
    /// A window went away (closed by the user inside tmux).
    WindowClose { window_id: u32 },
    /// A window was renamed.
    WindowRenamed { window_id: u32, name: String },
    /// The current session changed (user did `switch-client`).
    SessionChanged { session_id: u32, name: String },
    /// "Window list of the attached session changed somehow."
    /// A hint to refresh.  Carries no detail; the client should
    /// re-list windows.
    SessionsChanged,
    /// Active pane within a window changed.
    WindowPaneChanged { window_id: u32, pane_id: u32 },
    /// Beginning of a command response.  Pair with a later End/Error
    /// having the same `cmd_number`.
    Begin {
        cmd_number: u32,
    },
    /// End of a command response.  `output` accumulates everything
    /// between `%begin` and `%end` for this `cmd_number`.
    End {
        cmd_number: u32,
        output: Vec<u8>,
    },
    /// Command failed.  `output` is whatever lines tmux wrote between
    /// `%begin` and `%error`.
    CommandError {
        cmd_number: u32,
        output: Vec<u8>,
    },
    /// tmux is detaching the control client (server is exiting, or
    /// the user did `detach-client`).  Front-end should tear down.
    Exit { reason: Option<String> },
    /// A `%`-prefixed line we didn't recognise.  Surfaced rather than
    /// dropped so the front-end can log it (and we can grow handlers).
    Unknown(String),
}

#[derive(Debug, Default)]
pub struct Parser {
    /// Bytes accumulated since the last `\n`; processed when LF
    /// arrives.  Bounded by line length, so we don't grow unbounded
    /// on a hostile input stream (the protocol has no multi-MB
    /// single lines in practice).
    line_buf: Vec<u8>,
    /// When inside a `%begin ... %end` block, output between the
    /// markers accumulates here.  Set to `Some(cmd_number)` while
    /// inside the block, `None` outside.
    block: Option<u32>,
    block_buf: Vec<u8>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes from the tmux PTY into the parser.  Returns the
    /// (possibly empty) list of complete events that arrived in
    /// these bytes.  Bytes that don't yet form a full line are
    /// retained for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                self.process_line(&mut out);
                self.line_buf.clear();
            } else if b != b'\r' {
                // tmux uses LF terminators; some platforms send CRLF.
                // Drop CR bytes so they don't end up in event data.
                self.line_buf.push(b);
            }
        }
        out
    }

    fn process_line(&mut self, out: &mut Vec<Event>) {
        let line = std::mem::take(&mut self.line_buf);

        // Inside a %begin block: accumulate until %end / %error.
        if let Some(cmd) = self.block {
            if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
                let is_error = line.starts_with(b"%error ");
                if let Some(end_cmd) = parse_block_marker(&line) {
                    if end_cmd == cmd {
                        let output = std::mem::take(&mut self.block_buf);
                        if is_error {
                            out.push(Event::CommandError {
                                cmd_number: cmd,
                                output,
                            });
                        } else {
                            out.push(Event::End {
                                cmd_number: cmd,
                                output,
                            });
                        }
                        self.block = None;
                        return;
                    }
                }
                // Mismatched / malformed: drop the block, surface as Unknown.
                let dropped = std::mem::take(&mut self.block_buf);
                self.block = None;
                out.push(Event::Unknown(format!(
                    "block-mismatch cmd={cmd} lost {} bytes",
                    dropped.len()
                )));
                // Fall through to handle the current line normally.
            } else {
                // Plain line inside the block: accumulate, including a
                // line break that the consumer of `output` likely wants.
                self.block_buf.extend_from_slice(&line);
                self.block_buf.push(b'\n');
                return;
            }
        }

        if !line.starts_with(b"%") {
            // Outside a block, non-%-prefixed lines aren't expected
            // in control mode.  Surface for visibility.
            if !line.is_empty() {
                out.push(Event::Unknown(String::from_utf8_lossy(&line).into_owned()));
            }
            return;
        }

        // Dispatch on the first whitespace-delimited token.
        let mut iter = line.splitn(2, |b| *b == b' ');
        let head = iter.next().unwrap();
        let rest = iter.next().unwrap_or(&[]);

        match head {
            b"%begin" => {
                if let Some(cmd) = parse_block_marker(&line) {
                    self.block = Some(cmd);
                    self.block_buf.clear();
                    out.push(Event::Begin { cmd_number: cmd });
                } else {
                    out.push(Event::Unknown(
                        String::from_utf8_lossy(&line).into_owned(),
                    ));
                }
            }
            b"%output" => {
                if let Some(ev) = parse_output(rest) {
                    out.push(ev);
                } else {
                    out.push(Event::Unknown(
                        String::from_utf8_lossy(&line).into_owned(),
                    ));
                }
            }
            b"%window-add" => {
                if let Some(id) = parse_at_id(rest) {
                    out.push(Event::WindowAdd { window_id: id });
                }
            }
            b"%window-close" | b"%unlinked-window-close" => {
                if let Some(id) = parse_at_id(rest) {
                    out.push(Event::WindowClose { window_id: id });
                }
            }
            b"%window-renamed" | b"%unlinked-window-renamed" => {
                if let Some((id, name)) = parse_at_id_and_name(rest) {
                    out.push(Event::WindowRenamed {
                        window_id: id,
                        name,
                    });
                }
            }
            b"%session-changed" => {
                if let Some((id, name)) = parse_dollar_id_and_name(rest) {
                    out.push(Event::SessionChanged {
                        session_id: id,
                        name,
                    });
                }
            }
            b"%sessions-changed" => out.push(Event::SessionsChanged),
            b"%window-pane-changed" => {
                if let Some((win, pane)) = parse_window_pane_pair(rest) {
                    out.push(Event::WindowPaneChanged {
                        window_id: win,
                        pane_id: pane,
                    });
                }
            }
            b"%exit" => {
                let reason = if rest.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(rest).into_owned())
                };
                out.push(Event::Exit { reason });
            }
            _ => {
                out.push(Event::Unknown(
                    String::from_utf8_lossy(&line).into_owned(),
                ));
            }
        }
    }
}

/// Pull the `<cmd-number>` out of a `%begin/%end/%error` line.
/// Format: `%begin <ts> <cmd-number> <flags>` (sometimes more fields
/// in newer tmux; we just need cmd-number, the second integer).
fn parse_block_marker(line: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(line).ok()?;
    let mut parts = s.split_whitespace();
    parts.next()?; // %begin / %end / %error
    parts.next()?; // timestamp
    parts.next()?.parse().ok()
}

/// Parse `%<paneid> <data>` (the rest after `%output `).  pane id
/// starts with `%`.  data is octal-escaped; we unescape here.
fn parse_output(rest: &[u8]) -> Option<Event> {
    let mut iter = rest.splitn(2, |b| *b == b' ');
    let pane_token = iter.next()?;
    let data = iter.next().unwrap_or(&[]);
    if !pane_token.starts_with(b"%") {
        return None;
    }
    let id_str = std::str::from_utf8(&pane_token[1..]).ok()?;
    let pane_id: u32 = id_str.parse().ok()?;
    let bytes = unescape_output(data);
    Some(Event::Output { pane_id, bytes })
}

/// Parse `@<id>` from the start of `rest`, returning the integer.
fn parse_at_id(rest: &[u8]) -> Option<u32> {
    let token = rest
        .split(|b| *b == b' ')
        .next()
        .filter(|t| t.starts_with(b"@"))?;
    std::str::from_utf8(&token[1..]).ok()?.parse().ok()
}

/// Parse `@<id> <name…>`.  Name can contain spaces (tmux quotes
/// names with spaces, but we accept either form).
fn parse_at_id_and_name(rest: &[u8]) -> Option<(u32, String)> {
    let mut iter = rest.splitn(2, |b| *b == b' ');
    let id_token = iter.next()?;
    let name_bytes = iter.next().unwrap_or(&[]);
    if !id_token.starts_with(b"@") {
        return None;
    }
    let id: u32 = std::str::from_utf8(&id_token[1..]).ok()?.parse().ok()?;
    Some((id, String::from_utf8_lossy(name_bytes).into_owned()))
}

/// Parse `$<id> <name…>`.
fn parse_dollar_id_and_name(rest: &[u8]) -> Option<(u32, String)> {
    let mut iter = rest.splitn(2, |b| *b == b' ');
    let id_token = iter.next()?;
    let name_bytes = iter.next().unwrap_or(&[]);
    if !id_token.starts_with(b"$") {
        return None;
    }
    let id: u32 = std::str::from_utf8(&id_token[1..]).ok()?.parse().ok()?;
    Some((id, String::from_utf8_lossy(name_bytes).into_owned()))
}

/// Parse `@<window-id> %<pane-id>`.
fn parse_window_pane_pair(rest: &[u8]) -> Option<(u32, u32)> {
    let mut parts = rest.split(|b| *b == b' ');
    let win_t = parts.next()?;
    let pane_t = parts.next()?;
    if !win_t.starts_with(b"@") || !pane_t.starts_with(b"%") {
        return None;
    }
    let win: u32 = std::str::from_utf8(&win_t[1..]).ok()?.parse().ok()?;
    let pane: u32 = std::str::from_utf8(&pane_t[1..]).ok()?.parse().ok()?;
    Some((win, pane))
}

/// Reverse tmux's `%output` byte-encoding.  Non-printable bytes are
/// emitted as `\<3 octal digits>` (e.g. `\033` for ESC, `\012` for
/// LF).  A literal backslash is `\\`.  Anything else passes through.
fn unescape_output(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 1 < s.len() {
            if s[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if i + 3 < s.len()
                && (b'0'..=b'7').contains(&s[i + 1])
                && (b'0'..=b'7').contains(&s[i + 2])
                && (b'0'..=b'7').contains(&s[i + 3])
            {
                let h = s[i + 1] - b'0';
                let m = s[i + 2] - b'0';
                let l = s[i + 3] - b'0';
                out.push((h << 6) | (m << 3) | l);
                i += 4;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_one(input: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        p.feed(input)
    }

    #[test]
    fn output_with_octal_escape_decodes() {
        let evts = feed_one(b"%output %1 hello\\012world\n");
        assert_eq!(
            evts,
            vec![Event::Output {
                pane_id: 1,
                bytes: b"hello\nworld".to_vec()
            }]
        );
    }

    #[test]
    fn literal_backslash_decodes() {
        let evts = feed_one(b"%output %2 path\\\\to\\\\file\n");
        assert_eq!(
            evts,
            vec![Event::Output {
                pane_id: 2,
                bytes: b"path\\to\\file".to_vec()
            }]
        );
    }

    #[test]
    fn window_add_close_renamed() {
        let mut p = Parser::new();
        let evts = p.feed(b"%window-add @1\n%window-renamed @1 main\n%window-close @1\n");
        assert_eq!(
            evts,
            vec![
                Event::WindowAdd { window_id: 1 },
                Event::WindowRenamed {
                    window_id: 1,
                    name: "main".into()
                },
                Event::WindowClose { window_id: 1 },
            ]
        );
    }

    #[test]
    fn session_changed_and_pane_changed() {
        let mut p = Parser::new();
        let evts = p.feed(b"%session-changed $0 default\n%window-pane-changed @1 %3\n");
        assert_eq!(
            evts,
            vec![
                Event::SessionChanged {
                    session_id: 0,
                    name: "default".into()
                },
                Event::WindowPaneChanged {
                    window_id: 1,
                    pane_id: 3
                },
            ]
        );
    }

    #[test]
    fn begin_end_wraps_command_output() {
        let mut p = Parser::new();
        let evts = p.feed(
            b"%begin 1700000000 7 0\nwindow 1\nwindow 2\n%end 1700000000 7 0\n",
        );
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0], Event::Begin { cmd_number: 7 });
        let Event::End { cmd_number, output } = &evts[1] else {
            panic!("expected End, got {:?}", evts[1]);
        };
        assert_eq!(*cmd_number, 7);
        assert_eq!(output, b"window 1\nwindow 2\n");
    }

    #[test]
    fn begin_error_signals_command_failure() {
        let mut p = Parser::new();
        let evts = p
            .feed(b"%begin 1700000000 9 0\nbad command\n%error 1700000000 9 0\n");
        assert_eq!(evts.len(), 2);
        assert!(matches!(evts[0], Event::Begin { cmd_number: 9 }));
        assert!(matches!(
            &evts[1],
            Event::CommandError { cmd_number: 9, output } if output == b"bad command\n"
        ));
    }

    #[test]
    fn split_across_feeds_works() {
        let mut p = Parser::new();
        // Feed half a %output line, then the rest.
        let mut evts = p.feed(b"%output %1 par");
        assert!(evts.is_empty());
        evts = p.feed(b"tial\\012more\n");
        assert_eq!(
            evts,
            vec![Event::Output {
                pane_id: 1,
                bytes: b"partial\nmore".to_vec()
            }]
        );
    }

    #[test]
    fn unknown_lines_are_surfaced() {
        let evts = feed_one(b"%mystery-event some args\n");
        assert_eq!(evts.len(), 1);
        assert!(matches!(&evts[0], Event::Unknown(s) if s.starts_with("%mystery-event")));
    }

    #[test]
    fn exit_event() {
        let evts = feed_one(b"%exit server-exited\n");
        assert_eq!(
            evts,
            vec![Event::Exit {
                reason: Some("server-exited".into())
            }]
        );
    }

    #[test]
    fn cr_is_dropped_so_crlf_works() {
        let evts = feed_one(b"%window-add @5\r\n");
        assert_eq!(evts, vec![Event::WindowAdd { window_id: 5 }]);
    }

    #[test]
    fn block_with_pane_output_inside_is_accumulated_verbatim() {
        // Tmux command responses can contain arbitrary text; the parser
        // shouldn't re-interpret %-prefixed lines that arrive inside a
        // %begin block.  (In practice tmux doesn't produce nested
        // notifications mid-block, but we should at least not panic.)
        let mut p = Parser::new();
        let evts = p.feed(
            b"%begin 1700 1 0\nplain content\n%window-add @99\n%end 1700 1 0\n",
        );
        // Our impl currently treats %window-add inside the block as
        // part of the command output (no nested dispatch).  That
        // matches tmux's actual behaviour.
        let Event::End { output, .. } = &evts.last().unwrap() else {
            panic!("expected End");
        };
        assert!(output.contains_str("plain content"));
        assert!(output.contains_str("%window-add @99"));
    }

    /// Tiny helper trait to make the slice test above readable.
    trait ContainsStr {
        fn contains_str(&self, needle: &str) -> bool;
    }
    impl ContainsStr for Vec<u8> {
        fn contains_str(&self, needle: &str) -> bool {
            self.windows(needle.len()).any(|w| w == needle.as_bytes())
        }
    }
}
