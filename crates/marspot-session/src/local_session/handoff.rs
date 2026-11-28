//! Quiescing a session before an execv hands it to the next image.

use std::thread;
use std::time::{Duration, Instant};

use marspot_term::bytelog::ByteLog;

use super::LocalSession;

/// Longest an execv waits for typed input to reach the PTY.  A child
/// that does not read its stdin never takes it; the rest is lost.
const HANDOFF_INPUT_WAIT: Duration = Duration::from_millis(200);

impl LocalSession {
    /// Before an execv: let nothing in flight die with this image.
    ///
    /// The reader stops between two reads and everything it queued is
    /// parsed into the terminal the snapshot is taken from, held output
    /// included -- the next image starts unheld -- and written to the
    /// bytelog, whose length the next image counts its position from.
    /// Input queued for the PTY, replies included, is given time to
    /// reach it; what the PTY has not given up stays for the next image.
    pub fn quiesce_for_handoff(&mut self) {
        if let Some(r) = self.reader.take() {
            r.stop();
        }
        self.hold_grid(false);
        self.pump();
        self.bytelog.iter_mut().for_each(ByteLog::flush);
        let deadline = Instant::now() + HANDOFF_INPUT_WAIT;
        while self.writer.backlog() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if self.writer.backlog() > 0 {
            marspot_term::lx_warn!(
                "l3.execv.input_not_written",
                "the PTY did not take queued input before the execv",
                session_id = self.id,
                bytes = self.writer.backlog() as u64
            );
        }
    }
}
