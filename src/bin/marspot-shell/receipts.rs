//! Proof that a line was submitted, from the program that submitted it.
//!
//! Everything else in `pty_op` infers: it pastes, watches the pane
//! repaint, sends a return, and then reads the screen to guess whether
//! the line is still sitting in the composer. Every one of those steps
//! is a statement about another program's rendering, and on
//! 2026-10-01 three panes sat with an unsent line while the log said
//! all three had landed. See `.dev/rfcs/20261001-input-delivery.md`.
//!
//! A receipt is not an inference. The agent's own submit hook says "I
//! submitted *this*", and marspot is in a position to receive that
//! because it spawned the pane: it mints a token, hands it over in the
//! pane's environment, and listens on a socket of its own.
//!
//! What identifies the submit is a **fingerprint of the submitted
//! text**, not a nonce marspot generated. A nonce cannot work: the
//! hook runs on *every* submit, including the ones the person types,
//! and nothing tells it which injection it belongs to. Measured
//! 2026-10-01, a `UserPromptSubmit` hook receives on stdin:
//!
//!     prompt, prompt_id, session_id, cwd, transcript_path,
//!     permission_mode, hook_event_name
//!
//! `prompt` is the text itself. marspot knows what it pasted, so it
//! waits for a receipt whose fingerprint matches -- which also tells
//! our injection apart from a line the person sent a moment earlier.
//!
//! **This socket has one verb.** It would have been less code to add a
//! variant to `CliRequest` and reuse `l1-cmd.sock`, but that socket can
//! `SendText` into any pane, and its path would then be sitting in the
//! environment of every process the pane ever runs -- the user's shell,
//! their scripts, whatever the agent spawns. The environment sweep in
//! `SESSION_ENV_PREFIXES` exists because four agents once inherited a
//! socket that was never theirs; handing one over deliberately is the
//! same defect with intent behind it.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marspot_term::{lx_info, lx_warn};

/// How long an unclaimed receipt is kept.
///
/// Long enough that a slow submit still finds its own receipt waiting,
/// short enough that the table cannot grow without bound -- a pane the
/// user drives by hand produces one of these per prompt, forever.
const RECEIPT_TTL: Duration = Duration::from_secs(60);

/// Above this many unclaimed receipts, the oldest go first.
///
/// The TTL alone is not a bound: a pane submitting faster than the TTL
/// would grow the table between sweeps. Nothing here may grow with
/// uptime.
const MAX_UNCLAIMED: usize = 256;

pub use marspot_term::submit_receipt::socket_path;

/// Receipts that have come in and not yet been claimed, plus the token
/// each pane was given.
#[derive(Default)]
pub struct Receipts {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// (sid, fingerprint) that have been reported, and when.
    seen: Vec<((u64, u64), Instant)>,
}

impl Receipts {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Has `sid` reported submitting text with this fingerprint?
    /// Claims it if so, so a replay is not a second submit.
    pub fn claim(&self, sid: u64, fingerprint: u64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        inner.seen.retain(|(_, at)| now.duration_since(*at) < RECEIPT_TTL);
        match inner.seen.iter().position(|(k, _)| *k == (sid, fingerprint)) {
            Some(i) => {
                inner.seen.remove(i);
                true
            }
            None => false,
        }
    }

    /// Credit a receipt to the pane its token names.
    ///
    /// The session id is read out of the token and then *checked*
    /// against it, rather than trusted: the id is a small number any
    /// pane knows, and without the check a pane could report submits
    /// for its neighbour by editing one character.
    fn record(&self, token: &str, fingerprint: u64) -> Option<u64> {
        let sid: u64 = token.split_once('-').and_then(|(s, _)| u64::from_str_radix(s, 16).ok())?;
        if !marspot_term::submit_receipt::token_names(sid, token) {
            return None;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        inner.seen.retain(|(_, at)| now.duration_since(*at) < RECEIPT_TTL);
        while inner.seen.len() >= MAX_UNCLAIMED {
            inner.seen.remove(0);
        }
        inner.seen.push(((sid, fingerprint), now));
        Some(sid)
    }

    #[cfg(test)]
    fn unclaimed(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).seen.len()
    }
}

/// `<token> <fingerprint>` on one line.  One verb, so there is no
/// command to parse and nothing to get wrong on the way in.
fn parse_line(line: &str) -> Option<(&str, u64)> {
    let (token, nonce) = line.trim_end().split_once(' ')?;
    if token.is_empty() {
        return None;
    }
    Some((token, nonce.trim().parse().ok()?))
}

fn handle(stream: UnixStream, receipts: &Receipts) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    // Bounded: a receipt is two short fields, and a peer that sends
    // more is not one.
    if BufReader::new(stream).take(512).read_line(&mut line).is_err() {
        return;
    }
    match parse_line(&line).and_then(|(t, n)| receipts.record(t, n).map(|sid| (sid, n))) {
        Some((sid, nonce)) => {
            lx_info!("shell.receipt.in", "a pane reported a submit", sid = sid, fp = nonce)
        }
        None => lx_warn!(
            "shell.receipt.rejected",
            "a receipt named no pane we issued a token to -- dropped"
        ),
    }
}

/// The `--settings` argument that gives a Claude pane a submit hook.
///
/// `--settings` loads **additional** settings (its own help says so),
/// so the person's own hooks, permissions and everything else in their
/// config are untouched -- this adds one more `UserPromptSubmit`
/// entry beside whatever is already there.
///
/// **The hook can never fail.** Claude blocks the submit when a
/// `UserPromptSubmit` hook exits non-zero -- measured, not guessed:
///
///     UserPromptSubmit operation blocked by hook:
///     [... --submit-receipt]: unknown option `--submit-receipt`
///
/// That is a pane the person cannot type into at all, and it is one
/// version skew away at any time: marspot replaces its own binaries
/// and re-execs, the bundle and `binaries/current` are different
/// paths, and a pane's claude outlives all of it. So the command ends
/// in `; exit 0` and sends both streams to /dev/null. A receipt that
/// does not arrive costs us a fallback to reading the screen; a hook
/// that fails costs the person their terminal.
///
/// `None` when marspot cannot name its own binary, or when the path
/// is one that cannot be quoted. A pane started without it simply has
/// no receipts.
/// **Nothing consumes a receipt today, so nothing asks for this.**
///
/// The one reader was the submit step's verification, and the sentence
/// a moved pane is told now travels on claude's command line -- there
/// is no production caller of `Step::submit` left, so no receipt is
/// ever looked up. What the hook still cost was real and measured: a
/// fork and exec of this binary on every prompt the person sends by
/// hand, median 7.6 ms, up to 25.9 ms.
///
/// The machinery below it stays. It is a few hundred lines and a
/// socket that costs nothing while unused, and the question it answers
/// -- "did that text actually get submitted" -- is the one any future
/// typing path has to answer. Re-enabling it is adding this argument
/// back at the three places that built a `claude` command line.
pub fn claude_settings_arg() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?;
    // It goes inside a JSON string and then inside shell single
    // quotes. Both of those have exactly one character they cannot
    // carry, and neither is escaped here -- a marspot installed at a
    // path like that is not one to guess about.
    if exe.contains(['"', '\'', '\\']) {
        return None;
    }
    let cmd = format!("{exe} --submit-receipt >/dev/null 2>&1; exit 0");
    Some(format!(
        r#"{{"hooks":{{"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"{cmd}"}}]}}]}}}}"#
    ))
}

pub fn serve(receipts: Arc<Receipts>) -> std::io::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new()
        .name("l1-receipts".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => handle(stream, &receipts),
                    Err(e) => {
                        lx_warn!("shell.receipt.accept_failed", &format!("{e}"));
                        break;
                    }
                }
            }
        })?;
    lx_info!(
        "shell.receipt.listening",
        "submit-receipt socket bound",
        path = path.display().to_string()
    );
    Ok(())
}

use marspot_term::submit_receipt::{TOKEN_VAR, fingerprint};

/// The `--submit-receipt` entry point: an agent's hook reporting that
/// it submitted something.
///
/// Dispatched before anything else in `main`, for the same reason
/// `--cc-statusline` is: this runs on every prompt the person sends,
/// so it opens no log stream, builds no window, and touches nothing
/// but stdin and one socket.
///
/// Always exits 0. A hook that fails loudly interrupts the person's
/// agent over a bookkeeping error that is ours, not theirs; marspot
/// notices the missing receipt on its own side and says so there.
pub fn report_from_stdin() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let Ok(v) = crate::plugins::handoff::json::parse(&input) else {
        return 0;
    };
    let Some(prompt) = v.str_at("prompt") else {
        return 0;
    };
    let Ok(token) = std::env::var(TOKEN_VAR) else {
        return 0;
    };
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return 0;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = writeln!(stream, "{} {}", token, fingerprint(prompt));
    0
}

use std::io::{Read as _, Write as _};

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the state root at a scratch dir so the secret, and the
    /// tokens derived from it, belong to this test.
    fn sandbox(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("marspot-rcpt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        // SAFETY: test code, single-threaded at this point.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        // L1 does this at startup; a token cannot be derived without
        // it, because deriving one no longer creates anything.
        marspot_term::submit_receipt::ensure_secret().expect("a secret");
        dir
    }

    fn token(sid: u64) -> String {
        marspot_term::submit_receipt::token_for(sid).expect("a token")
    }

    #[test]
    fn a_receipt_is_claimed_once_and_only_by_its_own_pane() {
        let dir = sandbox("claim");
        let r = Receipts::new();
        let t = token(7);
        assert_eq!(r.record(&t, 99), Some(7));

        assert!(!r.claim(8, 99), "another pane cannot claim it");
        assert!(r.claim(7, 99), "the pane it was issued to can");
        assert!(!r.claim(7, 99), "and only once -- a replay is not a second submit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A token nobody was issued names no pane, so the receipt is
    /// dropped rather than credited to a guess.
    /// A token nobody could have been issued names no pane, so the
    /// receipt is dropped rather than credited to a guess.
    #[test]
    fn an_unknown_token_reports_nothing() {
        let dir = sandbox("unknown");
        let r = Receipts::new();
        assert_eq!(r.record("not-a-token", 99), None, "no session id in it");
        assert_eq!(r.record("7-0000000000000000", 99), None, "right shape, wrong secret");
        assert_eq!(r.unclaimed(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The session id is read out of the token and then checked
    /// against it.  Editing the id to a neighbour's must not work --
    /// an id is a small number every pane knows.
    #[test]
    fn a_pane_cannot_report_for_its_neighbour_by_editing_the_id() {
        let dir = sandbox("neighbour");
        let r = Receipts::new();
        let mine = token(7);
        let forged = mine.replacen("7-", "8-", 1);
        assert_eq!(r.record(&forged, 99), None, "the id must match the token");
        assert_eq!(r.record(&mine, 99), Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing here may grow with uptime: a pane the user drives by
    /// hand reports a submit per prompt, forever, and nobody claims
    /// those.
    #[test]
    fn unclaimed_receipts_are_bounded() {
        let dir = sandbox("bounded");
        let r = Receipts::new();
        let t = token(7);
        for n in 0..(MAX_UNCLAIMED as u64 * 3) {
            r.record(&t, n);
        }
        assert!(
            r.unclaimed() <= MAX_UNCLAIMED,
            "{} unclaimed after {} reports",
            r.unclaimed(),
            MAX_UNCLAIMED * 3
        );
        // The newest survive: a submit that just happened is the one
        // somebody is about to ask about.
        assert!(r.claim(7, MAX_UNCLAIMED as u64 * 3 - 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole path, over a real socket: a hook's JSON goes in one
    /// end and a claim comes out the other.
    ///
    /// The unit tests above all call `record` directly, so between
    /// them and the thing that runs in production sit the socket, the
    /// line format, the JSON, and the fingerprint agreeing on both
    /// sides. Each of those has been the broken one in some other
    /// system.
    #[test]
    fn a_hooks_json_becomes_a_claim_over_the_socket() {
        use std::io::Write as _;
        let dir = sandbox("e2e");
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");

        let r = Receipts::new();
        let token = token(42);
        let served = Arc::clone(&r);
        let t = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handle(stream, &served);
            }
        });

        // What the client sends, built the way the client builds it.
        let prompt = "额度恢复了，如果有刚被打断的工作请继续";
        let mut c = UnixStream::connect(&path).expect("connect");
        writeln!(c, "{} {}", token, fingerprint(prompt)).expect("write");
        drop(c);
        t.join().expect("server thread");

        assert!(
            r.claim(42, fingerprint(prompt)),
            "the submit the hook reported is the one marspot asks about"
        );
        // And a different text is a different submit.
        assert!(!r.claim(42, fingerprint("something else")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Trailing whitespace is not a different submit: a composer may
    /// trim what it was handed, and a receipt differing only by a
    /// newline has to still match.
    #[test]
    fn the_fingerprint_ignores_the_edges() {
        assert_eq!(fingerprint("carry on"), fingerprint("  carry on\n"));
        assert_ne!(fingerprint("carry on"), fingerprint("carry on now"));
    }

    /// The settings argument has to be JSON, and it has to name this
    /// binary with the flag that reads a receipt.
    ///
    /// Parsed rather than pattern-matched, because a string that
    /// merely contains the right words can still be a broken document
    /// -- and a broken one makes Claude refuse to start, which is a
    /// pane the person cannot use.
    #[test]
    fn the_settings_argument_is_json_that_names_this_binary() {
        let arg = claude_settings_arg().expect("this binary has a quotable path");
        let v = crate::plugins::handoff::json::parse(&arg).expect("valid JSON");
        let cmd = v
            .at(&["hooks", "UserPromptSubmit"])
            .and_then(|a| a.arr().first())
            .and_then(|e| e.at(&["hooks"]))
            .and_then(|a| a.arr().first())
            .and_then(|h| h.str_at("command"))
            .expect("a command");
        assert!(cmd.contains(" --submit-receipt"), "{cmd:?}");
        assert!(
            cmd.starts_with(std::env::current_exe().unwrap().to_str().unwrap()),
            "it has to be this binary, not a name on PATH: {cmd:?}"
        );
        // And it survives the quoting it will go through.
        assert!(!arg.contains('\''), "single quotes cannot be quoted this way");
    }

    /// The hook has to exit 0 whatever happens to the binary it names.
    ///
    /// Claude blocks the submit when a `UserPromptSubmit` hook fails,
    /// so a pane whose marspot moved, was replaced by a version
    /// without this flag, or is simply missing would stop accepting
    /// typing altogether. Run as a shell would run it, against a
    /// binary that does not exist, and against one that rejects the
    /// flag.
    #[test]
    fn the_hook_cannot_block_the_person_from_typing() {
        let arg = claude_settings_arg().expect("a path");
        let v = crate::plugins::handoff::json::parse(&arg).expect("valid JSON");
        let template = v
            .at(&["hooks", "UserPromptSubmit"])
            .and_then(|a| a.arr().first())
            .and_then(|e| e.at(&["hooks"]))
            .and_then(|a| a.arr().first())
            .and_then(|h| h.str_at("command"))
            .expect("a command")
            .to_string();

        let run = |cmd: &str| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .expect("sh runs")
        };

        // The real one, with no socket listening: still 0.
        let out = run(&template);
        assert!(out.status.success(), "a receipt nobody is listening for: {out:?}");

        // A marspot that is not there -- the version-skew case.
        let gone = template.replacen(
            std::env::current_exe().unwrap().to_str().unwrap(),
            "/nonexistent/marspot-shell",
            1,
        );
        let out = run(&gone);
        assert!(out.status.success(), "a binary that is gone: {out:?}");
        assert!(out.stderr.is_empty(), "and it says nothing to the person");

        // One that rejects the flag -- an older marspot.
        let old = template.replacen(
            std::env::current_exe().unwrap().to_str().unwrap(),
            "/bin/ls --definitely-not-a-flag",
            1,
        );
        assert!(run(&old).status.success(), "a marspot too old to know the flag");
    }

    #[test]
    fn the_line_format_refuses_what_it_cannot_read() {
        assert_eq!(parse_line("abc 42\n"), Some(("abc", 42)));
        assert_eq!(parse_line("abc 42"), Some(("abc", 42)));
        assert_eq!(parse_line("abc"), None, "no nonce");
        assert_eq!(parse_line(" 42"), None, "no token");
        assert_eq!(parse_line("abc notanumber"), None);
        assert_eq!(parse_line(""), None);
    }
}
