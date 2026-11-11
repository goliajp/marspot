//! usage — marspot's own account usage feed.
//!
//! The Claude and Codex plugins choose which account a pane runs on
//! from two feed files an external collector writes.  On a machine
//! without that collector, or once it has stopped, this plugin collects
//! the same numbers and writes them in the same schema under marspot's
//! state directory, where `marspot::cc_usage` looks next.  While the
//! external feed is fresh it does nothing.  Kimi accounts have no
//! external collector; their feed is always marspot's own.
//!
//! Everything happens on the plugin's own thread: a pass reads the
//! keychain, runs curl and starts an app-server per codex home, any of
//! which can take seconds.

mod claude;
mod codex;
mod feed;
mod http;
mod kimi;
mod sha256;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use marspot::cc_usage;
use marspot_term::{lx_info, lx_warn};

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use feed::Account;

/// Long enough after launch that the first pass does not compete with
/// the panes coming up.
const FIRST_PASS_AFTER: Duration = Duration::from_secs(10);
const PASS_EVERY: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct UsagePlugin {
    /// Dropping it ends the collector at its next wake.
    stop: Option<Sender<()>>,
}

impl UsagePlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for UsagePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata { name: "usage", api_version: PLUGIN_API_VERSION, tick_interval_ms: 0 }
    }

    fn start(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // a sandbox shares the developer's HOME: its keychain, its
        // logins and its rate limits are not a test's to spend.  What
        // it may ask is a kimi API on this machine, a test's stand-in.
        let sandbox = std::env::var_os("MARSPOT_DEV_SANDBOX").is_some();
        if sandbox && !std::env::var("KIMI_CODE_BASE_URL").is_ok_and(|base| kimi::is_local(&base)) {
            return Ok(());
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Ok(());
        };
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("usage-feed".into())
            .spawn(move || run(home, rx, sandbox))
            .map_err(|e| PluginError::Other(format!("usage collector: {e}")))?;
        self.stop = Some(tx);
        host.log(LogLevel::Info, "start", "collector running");
        Ok(())
    }

    fn stop(&mut self, _host: &dyn PluginHost) {
        self.stop = None;
    }
}

fn run(home: PathBuf, stop: Receiver<()>, sandbox: bool) {
    let mut collector = Collector::new(
        Sources { claude: cc_usage::feed_path(), codex: cc_usage::codex_feed_path() },
        cc_usage::own_feed_path(),
        cc_usage::own_codex_feed_path(),
    );
    collector.kimi_only = sandbox;
    // a test cannot wait five minutes for the second pass
    let every = std::env::var("MARSPOT_USAGE_PASS_SECS").ok().filter(|_| sandbox).and_then(|s| s.parse().ok());
    let mut wait = FIRST_PASS_AFTER;
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(wait) {
        collector.pass(&home, now_unix());
        wait = every.map_or(PASS_EVERY, Duration::from_secs);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The external collector's two feed files.
struct Sources {
    claude: PathBuf,
    codex: PathBuf,
}

struct Collector {
    sources: Sources,
    own_claude: PathBuf,
    own_codex: PathBuf,
    claude: Vec<Account>,
    codex: Vec<Account>,
    /// Beside the other two feeds, as `kimi-usage.json`.
    own_kimi: PathBuf,
    kimi: Vec<Account>,
    kimi_state: kimi::Kimi,
    /// Ask only a kimi API on this machine, and nothing else.
    kimi_only: bool,
}

impl Collector {
    /// Starts from what the last run wrote, so an account that cannot
    /// be read right now still has its last numbers.
    fn new(sources: Sources, own_claude: PathBuf, own_codex: PathBuf) -> Collector {
        let held = |path: &Path| feed::load(&std::fs::read_to_string(path).unwrap_or_default());
        // where `marspot::kimi_usage` reads it
        let own_kimi = own_claude.with_file_name("kimi-usage.json");
        Collector {
            claude: held(&own_claude),
            codex: held(&own_codex),
            kimi: held(&own_kimi),
            kimi_state: kimi::Kimi::default(),
            kimi_only: false,
            sources,
            own_claude,
            own_codex,
            own_kimi,
        }
    }

    /// Each provider on its own: the external collector may be writing
    /// one feed and not the other.
    fn pass(&mut self, home: &Path, now: i64) {
        let waits = self.own_kimi.with_file_name("kimi-waits.json");
        self.kimi = self.kimi_state.pass(home, &waits, std::mem::take(&mut self.kimi), now, self.kimi_only);
        // a machine with no kimi home gets no kimi feed
        if !self.kimi.is_empty() {
            write("kimi", &self.own_kimi, &self.kimi, now);
        }
        if self.kimi_only {
            return;
        }
        if !cc_usage::devops_is_collecting(&self.sources.claude, now) {
            let waits = self.own_claude.with_file_name("claude-waits.json");
            self.claude = claude::pass(home, &waits, std::mem::take(&mut self.claude), now);
            write("claude", &self.own_claude, &self.claude, now);
        }
        if !cc_usage::devops_is_collecting(&self.sources.codex, now) {
            self.codex = codex::pass(home, std::mem::take(&mut self.codex), now);
            write("codex", &self.own_codex, &self.codex, now);
        }
    }
}

fn write(provider: &str, path: &Path, accounts: &[Account], now: i64) {
    match feed::write_atomic(path, &feed::render(accounts, now)) {
        Ok(()) => lx_info!("usage.feed.written", "own usage feed written", provider = provider, accounts = accounts.len()),
        Err(e) => lx_warn!("usage.feed.write_failed", &format!("{}: {e}", path.display()), provider = provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_290_114;

    /// A pass over a home with no accounts makes no request and starts
    /// no process, which is what lets a test run one.
    #[test]
    fn a_pass_stands_down_while_the_external_feed_is_fresh() {
        let dir = marspot_term::test_env::TempDir::new("usage-pass");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let sources = Sources { claude: dir.join("devops/claude.json"), codex: dir.join("devops/codex.json") };
        let (own_claude, own_codex) = (dir.join("usage/claude-usage.json"), dir.join("usage/codex-usage.json"));
        std::fs::create_dir_all(dir.join("devops")).unwrap();
        let fresh = format!(r#"{{"generated_at":"{}","accounts":[]}}"#, feed::iso_utc(NOW - 60));
        std::fs::write(&sources.claude, &fresh).unwrap();
        std::fs::write(&sources.codex, &fresh).unwrap();

        let mut c = Collector::new(sources, own_claude.clone(), own_codex.clone());
        c.pass(&home, NOW);
        assert!(!own_claude.exists() && !own_codex.exists(), "the external collector is at work");
        assert!(!c.own_kimi.exists(), "and there is no kimi home to write about");
        assert_eq!(c.own_kimi, dir.join("usage/kimi-usage.json"));

        // the codex feed goes stale; the claude one is still written
        let stale = format!(r#"{{"generated_at":"{}","accounts":[]}}"#, feed::iso_utc(NOW - 900));
        std::fs::write(&c.sources.codex, stale).unwrap();
        c.pass(&home, NOW);
        assert!(!own_claude.exists(), "each provider is judged by its own feed");
        let written = std::fs::read_to_string(&own_codex).expect("marspot took over the codex feed");
        assert!(cc_usage::feed_is_fresh(&written, NOW));

        std::fs::remove_file(&c.sources.claude).unwrap();
        c.pass(&home, NOW);
        assert!(own_claude.exists(), "no external feed at all: marspot collects");
    }

    #[test]
    fn a_restart_starts_from_the_feed_the_last_run_wrote() {
        let dir = marspot_term::test_env::TempDir::new("usage-restart");
        let own = dir.join("usage/claude-usage.json");
        let held = Account {
            name: "Claude 1".into(),
            email: "a@b".into(),
            seven_day: Some(feed::Window::new(0.42, Some(NOW + 3_600), NOW)),
            last_read: NOW,
            ..Account::default()
        };
        feed::write_atomic(&own, &feed::render(std::slice::from_ref(&held), NOW)).unwrap();
        let sources = Sources { claude: dir.join("none"), codex: dir.join("none") };
        let c = Collector::new(sources, own, dir.join("usage/codex-usage.json"));
        assert_eq!(c.claude, vec![held]);
        assert!(c.codex.is_empty());
    }
}
