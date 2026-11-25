//! Reading one server's `[mcp_servers.<name>]` table out of a Codex
//! `config.toml`, line by line, as far as the edits need it.

use std::ops::Range;

use super::codex_config::{APPROVAL, OPEN, UNREADABLE};
use super::codex_inline::table_len;
use super::codex_toml::{closes, header, opened_by, string_token};

/// The `[mcp_servers.<name>]` table of a config.
pub(super) struct Table {
    /// The `command` value, where it is in the text.
    pub(super) command: Range<usize>,
    /// Where the line of `command` ends, its line break included.
    pub(super) command_line_end: usize,
    /// The approval as written; empty when it is no one-line string.
    pub(super) approval: Option<String>,
    /// The `env` value: an inline table up to its closing brace, any
    /// other value up to the end of its line, line break left out.
    pub(super) env: Option<Range<usize>>,
    /// Whether the environment is written outside an `env = …` line:
    /// a table of its own, `[….env]`, or dotted keys, `env.K = …`.
    pub(super) env_table: bool,
    /// The table and its sub-tables, up to the next table, the comments
    /// that lead into that one left out.
    pub(super) block: Range<usize>,
}

type Scanned<T> = Result<T, &'static str>;

/// What the scan has seen so far.
struct Scan<'a> {
    text: &'a str,
    name: &'a str,
    ours: String,
    sub: String,
    /// The table the current line is in.
    table: String,
    seen: bool,
    command: Option<(Range<usize>, usize)>,
    approval: Option<String>,
    env: Option<Range<usize>>,
    env_table: bool,
    block: Option<(usize, Option<usize>)>,
}

/// The table of server `name` in `text`, if there is one.  Err when the
/// file defines the entry in a form other than a `[mcp_servers.<name>]`
/// table with a one-line string `command`.
pub(super) fn scan_named(text: &str, name: &str) -> Scanned<Option<Table>> {
    let ours = format!("mcp_servers.{name}");
    let sub = format!("{ours}.");
    let mut s = Scan {
        text,
        name,
        ours,
        sub,
        table: String::new(),
        seen: false,
        command: None,
        approval: None,
        env: None,
        env_table: false,
        block: None,
    };
    // the delimiter of a multi-line string the scan is inside of
    let mut open: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        if let Some(delim) = open {
            if closes(line, delim) {
                open = None;
            }
            continue;
        }
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if body.starts_with('[') {
            s.header(body, start)?;
        } else if let Some(eq) = line.find('=') {
            open = s.pair(line, start, eq, at)?;
        }
    }
    if open.is_some() {
        return Err(OPEN);
    }
    s.finish()
}

impl Scan<'_> {
    fn header(&mut self, body: &str, start: usize) -> Scanned<()> {
        self.table = header(body);
        if self.table == self.ours {
            if self.seen {
                return Err(UNREADABLE);
            }
            self.seen = true;
            self.block = Some((start, None));
        } else if let Some((_, end @ None)) = &mut self.block
            && !self.table.starts_with(&self.sub)
        {
            *end = Some(start);
        }
        self.env_table |= self.table == format!("{}.env", self.ours);
        Ok(())
    }

    /// One `key = value` line at `start`, its `=` at `eq` and its end at
    /// `end`.  Returns the multi-line string the value opens.
    fn pair(&mut self, line: &str, start: usize, eq: usize, end: usize) -> Scanned<Option<&'static str>> {
        let key: String = line[..eq].chars().filter(|c| !matches!(c, ' ' | '\t' | '"' | '\'')).collect();
        let value = &line[eq + 1..];
        // the same entry written as a dotted key or an inline table
        let dotted = match self.table.as_str() {
            "" => key == self.ours || key.starts_with(&self.sub),
            "mcp_servers" => key == self.name || key.strip_prefix(self.name).is_some_and(|k| k.starts_with('.')),
            _ => false,
        };
        if dotted {
            return Err(UNREADABLE);
        }
        if self.table == self.ours {
            let from = start + eq + 1 + (value.len() - value.trim_start().len());
            self.key(&key, value.trim_start(), from, end)?;
        }
        Ok(opened_by(value))
    }

    /// A key of the server's own table, its value `v` starting at `from`.
    fn key(&mut self, key: &str, v: &str, from: usize, end: usize) -> Scanned<()> {
        let token = || string_token(v).map(|len| from..from + len);
        match key {
            "command" => {
                let range = token().ok_or(UNREADABLE)?;
                if self.command.is_some() {
                    return Err(UNREADABLE);
                }
                self.command = Some((range, end));
            }
            _ if key == APPROVAL => {
                self.approval = Some(token().map_or(String::new(), |r| self.text[r].to_string()));
            }
            "env" if v.starts_with('{') => {
                // an inline table that goes on past its line is not rewritten
                let len = table_len(v).ok_or(UNREADABLE)?;
                self.env = Some(from..from + len);
            }
            "env" => self.env = Some(from..from + v.trim_end_matches(['\n', '\r']).len()),
            _ if key.starts_with("env.") => self.env_table = true,
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Scanned<Option<Table>> {
        if !self.seen {
            return Ok(None);
        }
        let (command, command_line_end) = self.command.ok_or(UNREADABLE)?;
        let text = self.text;
        let block = self.block.map_or(0..0, |(s, e)| without_trailing_comments(text, s..e.unwrap_or(text.len())));
        Ok(Some(Table { command, command_line_end, approval: self.approval, env: self.env, env_table: self.env_table, block }))
    }
}

/// `block` without the comment lines at its end, which lead into the
/// table after it.
fn without_trailing_comments(text: &str, block: Range<usize>) -> Range<usize> {
    let mut end = block.end;
    for line in text[block.clone()].split_inclusive('\n').rev() {
        if !line.trim_start().starts_with('#') {
            break;
        }
        end -= line.len();
    }
    block.start..end
}
