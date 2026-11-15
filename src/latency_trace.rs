//! Timestamps along the path from a key press to the frame that shows
//! it, written by the shell and the core to one file each.
//!
//! Compiled in only with the `latency-trace` feature; without it every
//! function here is empty and the shipped binaries carry nothing.  With
//! it, marks are taken once `MARSPOT_LATENCY_TRACE` names a path
//! prefix.  Every process reads the same clock (uptime, raw), so marks
//! from the two files order against each other.
//!
//! A record is 16 bytes, little-endian: stage `u8`, three bytes of
//! padding, `aux u32`, `t_ns u64`.  The stage numbers are the file
//! format; `bin/_latency_report.py` reads them.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// The time the system stamped on the key event.
    KeyEventTime = 1,
    /// The view's `keyDown:` was entered.
    KeyDown = 2,
    /// The shell queued the key for the core.
    KeySent = 3,
    /// The core's reader thread decoded the key.
    CoreKeyRead = 4,
    /// The core's loop handed the key to the pane's session.
    CoreKeyHandled = 5,
    /// A session said its grid changed.
    GridReady = 6,
    /// The loop began a pump that brought something in.
    PumpBegin = 7,
    PumpEnd = 8,
    /// `aux` is the window index for the render and settle stages.
    RenderBegin = 9,
    /// The frame is committed to the GPU.
    RenderCommitted = 10,
    /// The loop saw the frame complete.
    Settled = 11,
    /// The core queued the surface announcement for the shell.
    SurfaceSent = 12,
    /// The shell's reader thread decoded the announcement.
    ShellSurfaceRead = 13,
    /// The shell's main thread began presenting.
    PresentBegin = 14,
    /// The layer has the frame; the call returned.
    PresentEnd = 15,
    /// A capture of the app's own window, as the window server
    /// composites it, first differed from the one before the key.
    /// `aux` is how long that capture took, in microseconds: the
    /// change reached the screen some time inside it.
    OnScreen = 16,
    /// The shell reported a frame shown, at the display's refresh.
    ShownSent = 17,
    /// The core took that report for the frame it waited on.
    FrameShown = 18,
    /// Not times: `aux` is microseconds of the frame just committed.
    BuildUs = 32,
    PanesUs = 33,
    EncodeUs = 34,
    /// Of the frame that just settled.
    GpuExecUs = 35,
    /// `aux` is bytes copied into the instance buffer.
    UploadBytes = 36,
    /// `aux` is rows laid out again, of `RowsTotal`.
    RowsRebuilt = 37,
    RowsTotal = 38,
    /// Present stages inside the shell, microseconds.
    DrawableUs = 40,
    ScheduledUs = 41,
    /// `aux` is a hash of the pixels of the surface just presented.
    SurfaceHash = 42,
}

#[cfg(feature = "latency-trace")]
mod on {
    use super::Stage;
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Mutex;

    const UNKNOWN: u8 = 0;
    const OFF: u8 = 1;
    const ON: u8 = 2;

    static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
    static SINK: Mutex<Option<Sink>> = Mutex::new(None);

    struct Sink {
        file: File,
        buf: Vec<u8>,
    }

    /// Records held before a write: a burst of frames between two idles.
    const BUF_RECORDS: usize = 16 * 1024;

    fn open() -> u8 {
        let Some(prefix) = std::env::var_os("MARSPOT_LATENCY_TRACE") else {
            return OFF;
        };
        let exe = std::env::current_exe().ok();
        let name = exe
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let mut path = prefix;
        path.push(format!(".{name}.{}.bin", std::process::id()));
        let Ok(file) = File::create(&path) else {
            return OFF;
        };
        *SINK.lock().unwrap() = Some(Sink { file, buf: Vec::with_capacity(BUF_RECORDS * 16) });
        ON
    }

    /// Is tracing on?  The first call decides, from the environment.
    #[inline]
    pub fn enabled() -> bool {
        match STATE.load(Ordering::Relaxed) {
            ON => true,
            OFF => false,
            _ => {
                let s = open();
                STATE.store(s, Ordering::Relaxed);
                s == ON
            }
        }
    }

    /// Nanoseconds on the clock every mark uses.
    pub fn now_ns() -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid timespec for the call to fill
        unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    #[inline]
    pub fn mark(stage: Stage, aux: u32) {
        if enabled() {
            record(stage, aux, now_ns());
        }
    }

    /// A mark whose time was taken elsewhere, or a value that is not one.
    #[inline]
    pub fn mark_at(stage: Stage, aux: u32, t_ns: u64) {
        if enabled() {
            record(stage, aux, t_ns);
        }
    }

    fn record(stage: Stage, aux: u32, t_ns: u64) {
        let mut guard = SINK.lock().unwrap();
        let Some(sink) = guard.as_mut() else { return };
        sink.buf.push(stage as u8);
        sink.buf.extend_from_slice(&[0; 3]);
        sink.buf.extend_from_slice(&aux.to_le_bytes());
        sink.buf.extend_from_slice(&t_ns.to_le_bytes());
        if sink.buf.len() >= BUF_RECORDS * 16 {
            let _ = sink.file.write_all(&sink.buf);
            sink.buf.clear();
        }
    }

    /// Write out what is held.  Called where a process is about to wait
    /// with nothing owed, so the write never sits inside a measured span.
    pub fn flush() {
        flush_over(0);
    }

    /// Write out what is held once it is more than `records`.
    pub fn flush_over(records: usize) {
        if !enabled() {
            return;
        }
        let mut guard = SINK.lock().unwrap();
        let Some(sink) = guard.as_mut() else { return };
        if sink.buf.len() <= records * 16 {
            return;
        }
        let _ = sink.file.write_all(&sink.buf);
        sink.buf.clear();
    }
}

#[cfg(feature = "latency-trace")]
pub use on::{enabled, flush, flush_over, mark, mark_at, now_ns};

#[cfg(not(feature = "latency-trace"))]
mod off {
    use super::Stage;

    #[inline(always)]
    pub fn enabled() -> bool {
        false
    }
    #[inline(always)]
    pub fn now_ns() -> u64 {
        0
    }
    #[inline(always)]
    pub fn mark(_stage: Stage, _aux: u32) {}
    #[inline(always)]
    pub fn mark_at(_stage: Stage, _aux: u32, _t_ns: u64) {}
    #[inline(always)]
    pub fn flush() {}
    #[inline(always)]
    pub fn flush_over(_records: usize) {}
}

#[cfg(not(feature = "latency-trace"))]
pub use off::{enabled, flush, flush_over, mark, mark_at, now_ns};
