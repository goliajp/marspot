//! The production op environment.

use std::sync::Arc;
use std::time::SystemTime;

use marspot::pidtree::{self, ProcId, SignalError};

use super::{OpEnv, PtyIo};

/// The production [`OpEnv`]: real signals, the real process table, the
/// real clock, and the pane's own bytelog as the output counter.
pub struct RealEnv {
    io: Arc<dyn PtyIo>,
    /// Where a pane's own report of a submit arrives, when it has one.
    ///
    /// Optional because a shell that could not bind the socket still
    /// runs the terminal; those panes fall back to reading the screen,
    /// which is what every pane did before.
    receipts: Option<Arc<crate::receipts::Receipts>>,
}

impl RealEnv {
    #[cfg(test)]
    pub fn new(io: Arc<dyn PtyIo>) -> Self {
        Self { io, receipts: None }
    }

    pub fn with_receipts(
        io: Arc<dyn PtyIo>,
        receipts: Option<Arc<crate::receipts::Receipts>>,
    ) -> Self {
        Self { io, receipts }
    }
}

impl OpEnv for RealEnv {
    fn submit_receipt(&self, sid: u64, text: &str) -> Option<bool> {
        let r = self.receipts.as_ref()?;
        Some(r.claim(sid, marspot_term::submit_receipt::fingerprint(text)))
    }

    fn io(&self) -> &Arc<dyn PtyIo> {
        &self.io
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn signal(&self, id: ProcId, sig: i32) -> Result<(), SignalError> {
        pidtree::signal(id, sig)
    }
    fn pid_alive(&self, id: ProcId) -> bool {
        // signal 0 asks without sending; only `Gone` is an answer of "no"
        !matches!(pidtree::signal(id, 0), Err(SignalError::Gone))
    }
    fn find_descendant(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        // an op waits on, or is about to type at, what it just changed
        let obs = crate::observe::shared().fresh(crate::observe::NOW);
        let found = obs.table.descendants(under).into_iter().find(|r| pred(r));
        found.map(pidtree::ProcRow::id)
    }
    fn output_len(&self, sid: u64) -> u64 {
        // L3 appends every byte the PTY produces to the session's
        // bytelog, so its length is the cheapest "has anything been
        // drawn" counter there is — one `stat`, and only while an op
        // is running.
        marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog")
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
    }
    fn output_tail(&self, sid: u64, bytes: usize) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let path = marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog");
        let Ok(mut f) = std::fs::File::open(&path) else { return Vec::new() };
        let Ok(len) = f.metadata().map(|m| m.len()) else { return Vec::new() };
        let from = len.saturating_sub(bytes as u64);
        if f.seek(SeekFrom::Start(from)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::with_capacity(bytes.min(len as usize));
        let _ = f.take(bytes as u64).read_to_end(&mut buf);
        buf
    }
    fn pane_modes(&self, sid: u64) -> Option<(u64, u32)> {
        // Same route as `output_len`: read what the session left on
        // disk.  The registry entry names the region; the region's
        // header carries the flags.  Read-only, header-sized, and only
        // while an op is running.
        let entry = marspot_term::session_registry::read_session_entry(sid).ok()?;
        let name = std::ffi::CString::new(entry.shm_name).ok()?;
        marspot_term::grid_shm::peek(&name).ok()
    }
    fn pane_screen(&self, sid: u64) -> Option<String> {
        marspot::pane_read::screen(sid, 0).ok()
    }
    fn pane_unsent(&self, sid: u64) -> Option<String> {
        marspot::pane_read::composer(sid).ok()?
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::super::{OpOutcome, OpRunner, PtyOp, Step};
    use super::*;
    use crate::plugins::{LogLevel, PaneSession, PaneSessionHost};

    struct NoIo;
    impl PtyIo for NoIo {
        fn send(&self, _: u64, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn hold(&self, _: u64, _: bool) -> std::io::Result<()> {
            Ok(())
        }
        fn paste(&self, _: u64, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Host {
        ended: Mutex<bool>,
        tags: Mutex<Vec<String>>,
    }
    impl PaneSessionHost for Host {
        fn shelld_session_id(&self) -> u64 {
            1
        }
        fn end(&self) {
            *self.ended.lock().unwrap() = true;
        }
        fn set_badge(&self, _: &str) {}
        fn log(&self, _: LogLevel, tag: &str, _: &str) {
            self.tags.lock().unwrap().push(tag.to_string());
        }
    }

    fn sleeper() -> Child {
        Command::new("/bin/sleep").arg("30").spawn().expect("spawn /bin/sleep")
    }

    /// Ticks until the runner asks to end, then ends it the way the
    /// host does, and returns the outcome.
    fn drive(op: PtyOp, host: &Host, limit: Duration) -> Option<OpOutcome> {
        let out = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&out);
        let mut r = OpRunner::new(op, Box::new(RealEnv::new(Arc::new(NoIo))))
            .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
        let deadline = Instant::now() + limit;
        while !*host.ended.lock().unwrap() && Instant::now() < deadline {
            r.on_tick(host);
            std::thread::sleep(Duration::from_millis(10));
        }
        r.on_end(host, crate::plugins::EndReason::PluginRequested);
        out.lock().unwrap().clone()
    }

    /// A pid that now names another process (a start time one
    /// microsecond off is what a recycled pid looks like) is not
    /// signalled, by the polite signal or the escalation, and the
    /// step ends as already gone.
    #[test]
    fn a_terminate_on_a_stale_id_signals_nothing() {
        let mut child = sleeper();
        let real = pidtree::proc_id(child.id() as i32).expect("child id");
        let stale = ProcId { pid: real.pid, start_us: real.start_us + 1 };
        let host = Host::default();
        let op = PtyOp::new("test.stale").step(
            Step::terminate(stale, libc::SIGKILL).escalate_after(Duration::ZERO, libc::SIGKILL),
        );
        let outcome = drive(op, &host, Duration::from_secs(5));
        assert_eq!(outcome, Some(OpOutcome::Done));
        assert!(host.tags.lock().unwrap().iter().any(|t| t == "test.stale.already_gone"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.try_wait().expect("try_wait").is_none(), "the child was signalled");
        assert_eq!(pidtree::proc_id(real.pid), Some(real));
        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    /// The same step with the right id takes the process down.
    #[test]
    fn a_terminate_on_the_live_id_ends_it() {
        let mut child = sleeper();
        let id = pidtree::proc_id(child.id() as i32).expect("child id");
        let reaper = std::thread::spawn(move || child.wait().expect("reap"));
        let host = Host::default();
        let op = PtyOp::new("test.live").step(Step::terminate(id, libc::SIGTERM));
        assert_eq!(drive(op, &host, Duration::from_secs(5)), Some(OpOutcome::Done));
        assert!(!reaper.join().unwrap().success(), "the child ended on the signal");
    }
}
