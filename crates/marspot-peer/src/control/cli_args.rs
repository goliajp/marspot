//! Word splitting shared by every `marspot-peer` command.  `--help` or
//! `-h` asks for the usage and an option the command does not know is a
//! usage mistake; both are decided here, before anything is sent.
//! Everything after `--` is taken whole, so a value that starts with
//! `-` goes there.

/// Why a command line asks nothing of the app.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// `--help` / `-h`: the usage goes to stdout, exit 0.
    Help,
    /// A mistake; the text says which (empty for a plain usage mistake).
    Usage(String),
}

impl From<String> for Stop {
    fn from(s: String) -> Self {
        Stop::Usage(s)
    }
}

impl From<&str> for Stop {
    fn from(s: &str) -> Self {
        Stop::Usage(s.into())
    }
}

/// The options one command knows.
pub struct Spec {
    pub flags: &'static [&'static str],
    pub values: &'static [&'static str],
    /// Once this many positional words are read, the rest of the line is
    /// taken whole (`pane run PANE COMMAND...`: the command's own flags).
    pub whole_after: Option<usize>,
    /// Once this many positional words are read, the next word is text
    /// whatever it looks like, and so is the rest of the line; only a
    /// `--` there still opens the tail (`send NAME TEXT...`).
    pub text_after: Option<usize>,
}

impl Spec {
    pub const fn flags(flags: &'static [&'static str]) -> Spec {
        Spec { flags, values: &[], whole_after: None, text_after: None }
    }
}

#[derive(Debug, Default)]
pub struct Args {
    flags: Vec<&'static str>,
    values: Vec<(&'static str, String)>,
    pub pos: Vec<String>,
    /// The words after `--`, when there was one.
    pub tail: Option<Vec<String>>,
}

impl Args {
    pub fn flag(&self, f: &str) -> bool {
        self.flags.contains(&f)
    }

    /// The value given to `name`; the last one when given twice.
    pub fn value(&self, name: &str) -> Option<String> {
        self.values.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| v.clone())
    }

    /// The positional words, then the words after `--`.
    pub fn words(&self) -> Vec<String> {
        let mut w = self.pos.clone();
        w.extend(self.tail.iter().flatten().cloned());
        w
    }

    /// The words after the first one (a command's target), flags dropped.
    pub fn after_first(&self) -> Args {
        match self.pos.split_first() {
            Some((_, pos)) => Args { pos: pos.to_vec(), tail: self.tail.clone(), ..Default::default() },
            None => Args { tail: self.tail.as_ref().map(|t| t.iter().skip(1).cloned().collect()), ..Default::default() },
        }
    }

    /// Whether the words are a lone `-`, the sign for stdin (a `-` after
    /// `--` is the text itself).
    pub fn stdin_sign(&self) -> bool {
        self.tail.is_none() && self.pos.len() == 1 && self.pos[0] == "-"
    }
}

/// A word an option parser reads as an option: `-` and a name, no
/// spaces (`"- item"` or a lone `-` is a value).
pub fn looks_like_option(s: &str) -> bool {
    s.len() > 1 && s.starts_with('-') && !s.contains(char::is_whitespace)
}

/// Split `args` by `spec`.  A help request anywhere outside a value and
/// the tail wins over a mistake found earlier.
pub fn split(args: &[String], spec: &Spec) -> Result<Args, Stop> {
    let mut out = Args::default();
    let mut mistake = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if spec.whole_after.is_some_and(|n| out.pos.len() >= n) {
            out.pos.push(a.clone());
            continue;
        }
        if a != "--" && spec.text_after.is_some_and(|n| out.pos.len() >= n) {
            out.pos.push(a.clone());
            out.pos.extend(it.by_ref().cloned());
            break;
        }
        if a == "--" {
            out.tail = Some(it.by_ref().cloned().collect());
            break;
        }
        // once a command that ends in text has its target, a help word is
        // a mistake to report, not a request that would send nothing
        let after_target = (spec.whole_after.is_some() || spec.text_after.is_some()) && !out.pos.is_empty();
        if (a == "--help" || a == "-h") && !after_target {
            return Err(Stop::Help);
        }
        if let Some(f) = spec.flags.iter().find(|f| **f == a) {
            out.flags.push(f);
        } else if let Some(v) = spec.values.iter().find(|v| **v == a) {
            match it.next() {
                Some(val) => out.values.push((v, val.clone())),
                None => {
                    mistake.get_or_insert(format!("{a} needs a value"));
                }
            }
        } else if looks_like_option(a) {
            mistake.get_or_insert(format!("{a}: unknown option (a value that starts with - goes after --)"));
        } else {
            out.pos.push(a.clone());
        }
    }
    match mistake {
        Some(m) => Err(Stop::Usage(m)),
        None => Ok(out),
    }
}

/// What a command line whose first word `sub` names no command gets:
/// the usage when it asks for help anywhere before `--`.
pub fn unknown(sub: &str, rest: &[String]) -> Stop {
    let before_dash = rest.iter().take_while(|a| *a != "--");
    if sub == "--help" || sub == "-h" || before_dash.into_iter().any(|a| a == "--help" || a == "-h") {
        return Stop::Help;
    }
    if looks_like_option(sub) {
        return Stop::Usage(format!("{sub}: unknown option"));
    }
    Stop::Usage(format!("{sub:?}: no such command"))
}

/// Print what a stopped command line gets; the exit code.
pub fn report(stop: Stop, who: &str, usage: &str, usage_code: u8) -> u8 {
    match stop {
        Stop::Help => {
            println!("{usage}");
            0
        }
        Stop::Usage(e) => {
            if !e.is_empty() {
                eprintln!("{who}: {e}");
            }
            eprintln!("{usage}");
            usage_code
        }
    }
}
