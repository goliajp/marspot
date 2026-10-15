//! Shell command lines typed into a pane on the user's behalf.

/// A shell command line built from parts that are checked, not hoped
/// about.
///
/// The hand-written version of this was `format!("CLAUDE_CONFIG_DIR='{}'
/// claude --resume {}\r", dir, uuid)` with a separate "is this dir
/// safe?" predicate that the caller had to remember to call.  Here the
/// check is the constructor: an unsafe value cannot reach the line.
pub struct PtyCommand {
    program: String,
    /// In the order they were given.  A flag and its value are
    /// adjacent on the line or they are not a flag and its value --
    /// keeping quoted arguments in a list of their own put
    /// `--settings` next to `--resume` and the document at the end.
    args: Vec<Arg>,
    env: Vec<(String, String)>,
    /// Where the line runs, when it matters which directory that is.
    dir: Option<String>,
    clear_first: bool,
}

enum Arg {
    Bare(String),
    Quoted(String),
}

impl PtyCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            dir: None,
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            clear_first: false,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(Arg::Bare(a.into()));
        self
    }

    /// An argument wrapped in single quotes.
    ///
    /// `shell_safe` rejects the whole class of expansion characters,
    /// which is right for a bare word and wrong for a JSON document --
    /// `--settings '{"hooks":…}'` is a legitimate argument that can
    /// never pass it. Inside single quotes a shell expands nothing, so
    /// the only character that matters is the single quote itself, and
    /// that is still refused rather than escaped. Same rule as `env`,
    /// narrower proof.
    pub fn quoted_arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(Arg::Quoted(a.into()));
        self
    }

    /// `KEY='value'` in front of the command.  A value that could break
    /// out of its quoting is **refused**, not escaped: a path that
    /// strange is not one to guess about, and the caller gets `None`
    /// rather than a line that does something else.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Run the command from `dir`.
    ///
    /// For a command that resumes something: a `claude --resume` is
    /// answered from the shell's working directory, and claude binds
    /// the session it reopens to *that* directory -- its project rules
    /// and its hooks come from there. A pane whose shell has been
    /// `cd`-ed somewhere else since the session started therefore
    /// reopens it under a different project, and the only sign is that
    /// the pane's title changes. Seen on the real machine: a torajs
    /// session came back as `devops`, loading devops's rules.
    ///
    /// A directory that could break out of its quoting is refused the
    /// way an env value is, and a `None` leaves the line as it was --
    /// running where the shell stands, which is today's behaviour.
    pub fn in_dir(mut self, dir: Option<impl Into<String>>) -> Self {
        self.dir = dir.map(Into::into);
        self
    }

    /// Wipe the screen as the line's first act.
    ///
    /// The typed line is echoed by the shell; behind a screen hold
    /// nobody watches that happen, but it is still there when the hold
    /// lifts.  Erasing the display first leaves the pane showing only
    /// what the command itself paints.  `\033[2J` and nothing else —
    /// `\033[3J` would take the scrollback with it, and the scrollback
    /// is the user's.
    pub fn clear_screen_first(mut self, on: bool) -> Self {
        self.clear_first = on;
        self
    }

    /// The bytes to type, or `None` if any part is unsafe to quote.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        let mut line = String::new();
        if self.clear_first {
            line.push_str("printf '\\033[H\\033[2J'; ");
        }
        // A subshell, so the directory this runs in is not left behind
        // on the person's prompt: they did not ask to be moved. `&&`
        // and not `;` because a cd that fails must not be followed by a
        // resume in the wrong place, which is the thing this is for.
        // `exec` keeps the process tree the shape the scanner expects
        // -- the program a direct child of the pane's shell.
        if let Some(d) = &self.dir {
            if !quotable(d) {
                return None;
            }
            line.push_str(&format!("( cd '{d}' && "));
        }
        for (k, v) in &self.env {
            if !shell_safe(k) || !shell_safe(v) {
                return None;
            }
            line.push_str(&format!("{k}='{v}' "));
        }
        // After the assignments, never before them: `exec FOO=bar cmd`
        // asks the shell to run a program called `FOO=bar`, and both
        // sh and zsh answer "command not found". Written the wrong way
        // round, every switch ran a subshell that exited 127 and then
        // waited twenty seconds for a claude that was never started.
        if self.dir.is_some() {
            line.push_str("exec ");
        }
        if !shell_safe(&self.program) {
            return None;
        }
        line.push_str(&self.program);
        for a in &self.args {
            match a {
                Arg::Bare(a) => {
                    if !shell_safe(a) {
                        return None;
                    }
                    line.push(' ');
                    line.push_str(a);
                }
                Arg::Quoted(q) => {
                    // Everything a shell could act on is inert between
                    // single quotes; a single quote is not, and is
                    // refused rather than escaped.
                    if q.is_empty() || q.contains(['\'', '\n', '\r']) {
                        return None;
                    }
                    line.push_str(" '");
                    line.push_str(q);
                    line.push('\'');
                }
            }
        }
        if self.dir.is_some() {
            line.push_str(" )");
        }
        line.push('\r');
        Some(line.into_bytes())
    }
}

/// A value that survives being wrapped in single quotes.
///
/// Weaker than [`shell_safe`] on purpose, and for a different job: a
/// directory name may legitimately contain `$`, `&` or a space, and
/// inside single quotes none of them mean anything. What it cannot
/// contain is the quote itself, or a newline that would end the line.
pub fn quotable(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && !s.contains(['\'', '\n', '\r'])
}

/// A value that can go on a command line without changing its meaning.
///
/// Rejects rather than escapes, and rejects the whole class: quotes,
/// newlines, backslashes, and the three shells' expansion characters.
pub fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && !s.contains(['\'', '"', '\n', '\r', '\\', '$', '`', ';', '&', '|', '<', '>'])
}
