//! The first word of a `marspot-peer` command line, and the words of the
//! commands that are not `pane`, `helper` or `procs`.

use super::cli_args::{self, Spec, Stop};

pub const USAGE: &str = "usage:
  marspot-peer list
  marspot-peer whoami
  marspot-peer send [--now] <name | uds:address> <text... | - | -- TEXT...>
  marspot-peer mcp [--client-timeout-s N]
  marspot-peer relay <inbox> <upstream> <send>
  marspot-peer claude-hook
  marspot-peer gemini-hook
  marspot-peer kimi-hook
  marspot-peer pane ...    (see `marspot-peer pane --help`)
  marspot-peer helper ...  (see `marspot-peer helper --help`)
  marspot-peer procs ...   (see `marspot-peer procs --help`)
any of them takes -h / --help for its usage";

/// The exit code of a usage mistake outside `pane`, `helper` and `procs`
/// (kimi reads 2 from its hook as "keep the turn going").
pub const USAGE_CODE: u8 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum Top {
    /// `pane`, `helper` or `procs` and the words after it, parsed there.
    Control(&'static str, Vec<String>),
    List,
    Whoami,
    Send { now: bool, to: String, words: Vec<String>, stdin: bool },
    /// `--client-timeout-s` as written, and the words `mcp` does not
    /// know; both read by the caller.
    Mcp { client_timeout_s: Option<String>, ignored: Vec<String> },
    Relay { public: String, upstream: String, send: String },
    ClaudeHook,
    GeminiHook,
    KimiHook,
}

/// What the command line asks for.
pub fn parse(args: &[String]) -> Result<Top, Stop> {
    let Some((cmd, rest)) = args.split_first() else { return Err(Stop::Usage(String::new())) };
    let control = |name| Ok(Top::Control(name, rest.to_vec()));
    let spec = match cmd.as_str() {
        "pane" => return control("pane"),
        "helper" => return control("helper"),
        "procs" => return control("procs"),
        // options go before the name; after it every word is text
        "send" => Spec { flags: &["--now"], values: &[], whole_after: None, text_after: Some(1) },
        "mcp" => return mcp(rest),
        "list" | "whoami" | "relay" | "claude-hook" | "gemini-hook" | "kimi-hook" => Spec::flags(&[]),
        other => return Err(cli_args::unknown(other, rest)),
    };
    let a = cli_args::split(rest, &spec)?;
    let words = a.words();
    let top = match (cmd.as_str(), &words[..]) {
        ("list", []) => Top::List,
        ("whoami", []) => Top::Whoami,
        ("claude-hook", []) => Top::ClaudeHook,
        ("gemini-hook", []) => Top::GeminiHook,
        ("kimi-hook", []) => Top::KimiHook,
        ("relay", [p, u, s]) => Top::Relay { public: p.clone(), upstream: u.clone(), send: s.clone() },
        ("send", [_, _, ..]) => {
            let text = a.after_first();
            Top::Send { now: a.flag("--now"), to: words[0].clone(), stdin: text.stdin_sign(), words: text.words() }
        }
        _ => return Err(Stop::Usage(String::new())),
    };
    Ok(top)
}

/// `mcp` is started from agents' own config files, which marspot keeps
/// as their owner wrote them (`args = ["mcp", "-v"]`): a word it does
/// not know is passed back to be warned about, never a reason to refuse.
fn mcp(rest: &[String]) -> Result<Top, Stop> {
    let (mut client_timeout_s, mut ignored) = (None, Vec::new());
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Err(Stop::Help),
            "--client-timeout-s" => match it.next() {
                Some(n) => client_timeout_s = Some(n.clone()),
                None => ignored.push(a.clone()),
            },
            _ => ignored.push(a.clone()),
        }
    }
    Ok(Top::Mcp { client_timeout_s, ignored })
}

#[cfg(test)]
#[path = "cli_help_tests.rs"]
mod cli_help_tests;
