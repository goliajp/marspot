//! Whether what the pane's program prints may be its answer to a key.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long after a key what its program prints is handed over as it
/// comes, flood or not.
const ECHO_FOR: Duration = Duration::from_millis(100);

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

    pub(super) fn expected(&self) -> bool {
        let at = self.0.load(Ordering::SeqCst);
        if at == 0 {
            return false;
        }
        let expected = clock().saturating_sub(at) < ECHO_FOR.as_nanos() as u64;
        if !expected {
            // so a flood reads no clock until the next key
            let _ = self.0.compare_exchange(at, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        expected
    }

    /// An echo expected for as long as the test runs, however long the
    /// machine keeps it waiting.
    #[cfg(test)]
    pub(super) fn always() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}
