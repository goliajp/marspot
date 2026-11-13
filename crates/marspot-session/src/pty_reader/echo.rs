//! Whether what the pane's program prints may be its answer to a key.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long after a key its pane's reader waits for no turn among
/// flooding panes.
const ECHO_FOR: Duration = Duration::from_millis(100);

/// How long after a key what its program prints is handed over as soon
/// as the parser can take it, flood or not.
const AWAITED_FOR: Duration = crate::publish_gate::FloodGate::ECHO_WINDOW;

/// How long after a key it is handed over each time the pty runs dry.
const AT_ONCE_FOR: Duration = crate::publish_gate::FloodGate::ECHO_SOON;

/// When a key was last typed, on [`clock`]; 0 for never.
pub(super) struct Echo(AtomicU64);

/// Nanoseconds since this process first asked, never 0.
fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

impl Echo {
    pub(super) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// A key is on its way to the pane's program.
    pub(super) fn typed(&self) {
        self.0.store(clock(), Ordering::SeqCst);
    }

    /// Nanoseconds since the last key, while its echo is expected.
    fn since_key(&self) -> Option<u64> {
        let at = self.0.load(Ordering::SeqCst);
        if at == 0 {
            return None;
        }
        let since = clock().saturating_sub(at);
        if since >= ECHO_FOR.as_nanos() as u64 {
            // so a flood reads no clock until the next key
            let _ = self.0.compare_exchange(at, 0, Ordering::SeqCst, Ordering::SeqCst);
            return None;
        }
        Some(since)
    }

    pub(super) fn expected(&self) -> bool {
        self.since_key().is_some()
    }

    /// The key was typed so recently that its answer is awaited: `Some`,
    /// and `true` while it may be in the very next bytes read.
    pub(super) fn awaited(&self) -> Option<bool> {
        #[cfg(test)]
        if self.0.load(Ordering::SeqCst) == Self::LATER {
            return Some(false);
        }
        let since = self.since_key().filter(|&since| since < AWAITED_FOR.as_nanos() as u64)?;
        Some(since < AT_ONCE_FOR.as_nanos() as u64)
    }

    /// An answer awaited for as long as the test runs, and never in the
    /// very next bytes.
    #[cfg(test)]
    pub(super) fn later() -> Self {
        Self(AtomicU64::new(Self::LATER))
    }

    #[cfg(test)]
    const LATER: u64 = u64::MAX - 1;

    /// An echo expected for as long as the test runs, however long the
    /// machine keeps it waiting.
    #[cfg(test)]
    pub(super) fn always() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}
