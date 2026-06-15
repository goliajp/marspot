//! Structured logging for marspot.
//!
//! Replaces the ad-hoc `eprintln!` scatter with one TSV stream shared by
//! every binary (shelld, shell, core, session, gui). Each line is
//! independently parseable — ISO-ms / unix-ms / level / component / pid /
//! tid / tag / msg / k=v — bounded by size + age rotation, gzipped on
//! rotate, and cold-data GC'd in the background. See `docs/logx.md` for
//! the operator runbook.
//!
//! Why TSV (not JSON): hot paths can't afford an allocation per event to
//! escape JSON; `grep` / `cut` / `awk` are right there in the same
//! terminal where marspot is built. The legacy `sup_log.rs` was already
//! TSV, so its 33 call sites migrate via a 10-line shim.
//!
//! Call-site convention (the only API end users touch):
//!
//! ```ignore
//! use marspot_term::lx_info;
//! lx_info!("session.reader.exit", "EOF on pty", id = id, bytes = total);
//! ```

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

pub mod gc;
pub mod rotate;
pub mod sink;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" | "warning" => Level::Warn,
            "error" | "err" => Level::Error,
            _ => return None,
        })
    }
}

/// `Info` by default. Set once in `init()` from the resolved env vars,
/// then read on every call via `should_log` — a single relaxed AtomicU8
/// compare on the hot path. No mutex, no allocation.
static GLOBAL_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static COMPONENT: OnceLock<&'static str> = OnceLock::new();

/// Cheap predicate the macros short-circuit on; everything after this
/// (including the `format!` cost of building `msg`) is bypassed when
/// the level is filtered out.
#[inline]
pub fn should_log(level: Level) -> bool {
    (level as u8) >= GLOBAL_LEVEL.load(Ordering::Relaxed)
}

/// Once-per-process startup. Sets the component tag, resolves env vars
/// into the level filter, opens the sink, and spawns a detached startup
/// GC sweep. Idempotent — a second call is a no-op so binaries that may
/// re-init across an execv (shelld) don't double up.
///
/// **Also installs the panic hook** unconditionally — any subsequent
/// panic anywhere in this process (main loop, any spawned thread)
/// lands a `PANIC` event in `marspot.log` with thread name, source
/// location, and a fully-captured backtrace split across `PANIC_BT`
/// frames.  Without this, panics default-write to stderr — which is
/// redirected to `/dev/null` for daemon-launched binaries and lost
/// forever, leaving only the supervisor's `CORE_EXIT` line and no
/// root cause.  This is THE forensic primitive marspot's debugging
/// loop hinges on.
pub fn init(component: &'static str) {
    if COMPONENT.set(component).is_err() {
        return;
    }
    // Per-component override wins over the global MARSPOT_LOG.
    let comp_var = format!("MARSPOT_LOG_{}", component.to_ascii_uppercase());
    let level = std::env::var(&comp_var)
        .ok()
        .and_then(|s| Level::parse(&s))
        .or_else(|| {
            std::env::var("MARSPOT_LOG")
                .ok()
                .and_then(|s| Level::parse(&s))
        })
        .unwrap_or(Level::Info);
    GLOBAL_LEVEL.store(level as u8, Ordering::Relaxed);
    let _ = sink::ensure_open();
    install_panic_hook();
    if std::env::var("MARSPOT_LOG_GC")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        std::thread::Builder::new()
            .name("marspot-logx-gc-startup".into())
            .spawn(move || gc::sweep_startup(component))
            .ok();
    }
}

/// Capture every panic to the structured log before the process dies.
///
/// Stable Rust's default panic hook prints to stderr; daemon-launched
/// marspot processes (shelld via LaunchAgent, the L2 core spawned with
/// `Stdio::null()` stderr) have nowhere for that to land.  The
/// supervisor only sees the child's exit and writes `CORE_EXIT`, which
/// is useless without the panic's payload + location + backtrace.
///
/// Strategy:
///   1.  One `PANIC` line with payload + thread + location, fields
///       short enough to fit under MAX_LINE.
///   2.  The full `Backtrace` split across multiple `PANIC_BT` lines
///       (one frame per line, indexed by `frame=N`) so a 480 B line
///       cap doesn't lose anything.  Reassemble with
///       `grep $'\tPANIC_BT\t' | sort -k? frame=`.
///   3.  After log lines land, fall through to the default hook so
///       stderr / DiagnosticReports still get the standard output —
///       belt-and-suspenders.
///
/// Idempotent — installed exactly once even if `init()` somehow
/// re-runs across an execv.
fn install_panic_hook() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        // Capture the existing hook so we can chain to it after logging.
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Payload: panics built via `panic!("...")` carry &'static str;
            // assertions and `panic!("{}", v)` may carry String; FFI
            // panics can carry anything.  Try both common types first.
            let payload = info.payload();
            let msg: String = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let thread_name = std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_string();
            // force_capture: stable Rust gates `Backtrace::capture()`
            // on RUST_BACKTRACE=1; force_capture ignores the env var
            // so production builds always get frames.  The performance
            // hit only matters in the panic path, by which point we're
            // dying anyway.
            let bt = std::backtrace::Backtrace::force_capture();
            event(
                Level::Error,
                "PANIC",
                &msg,
                &[
                    ("thread", &thread_name as &dyn fmt::Display),
                    ("loc", &location as &dyn fmt::Display),
                ],
            );
            // Split backtrace into one event per frame so MAX_LINE
            // doesn't truncate the middle of a long trace.  Cap at
            // 64 frames — deeper than any real marspot stack and
            // bounded so a runaway recursion's panic doesn't blow
            // the log up to GB.
            for (i, line) in format!("{}", bt).lines().take(64).enumerate() {
                event(
                    Level::Error,
                    "PANIC_BT",
                    line.trim(),
                    &[("frame", &i as &dyn fmt::Display)],
                );
            }
            // Chain to the original hook so DiagnosticReports / stderr
            // still get their standard output.
            default(info);
        }));
    });
}

/// Append one event. Cheap; never panics; level-filtered.
/// `fields` is a slice of `(key, &dyn Display)` — keys are tiny snake_case
/// literals, values are caller-formatted.
pub fn event(
    level: Level,
    tag: &str,
    msg: &str,
    fields: &[(&str, &dyn fmt::Display)],
) {
    if !should_log(level) {
        return;
    }
    LINE_BUF.with(|cell| {
        let mut buf = match cell.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => return, // re-entrant call inside Display impl; skip
        };
        buf.clear();
        format_line(&mut buf, level, tag, msg, fields);
        sink::write_line(buf.as_bytes());
    });
}

const MAX_LINE: usize = 480;
const TRUNC_TAIL: &str = "…trunc\n";

thread_local! {
    static LINE_BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
}

fn format_line(
    buf: &mut String,
    level: Level,
    tag: &str,
    msg: &str,
    fields: &[(&str, &dyn fmt::Display)],
) {
    use std::fmt::Write as _;
    let now_ms = now_unix_ms();
    let iso = iso8601_ms(now_ms);
    let comp = COMPONENT.get().copied().unwrap_or("?");
    let pid = std::process::id();
    let tid = thread_token();
    let _ = write!(
        buf,
        "{}\t{}\t{}\t{}\t{}\t{:x}\t{}\t",
        iso,
        now_ms,
        level.as_str(),
        comp,
        pid,
        tid,
        tag,
    );
    push_sanitised(buf, msg, /*also_space=*/ false);
    for (k, v) in fields {
        buf.push('\t');
        let _ = write!(buf, "{}=", k);
        // The field value's Display impl is given its own scratch buffer
        // so a panicking impl can't corrupt the main line buffer in place.
        let mut scratch = String::new();
        let _ = write!(scratch, "{}", v);
        push_sanitised(buf, &scratch, /*also_space=*/ true);
    }
    buf.push('\n');
    if buf.len() > MAX_LINE {
        // Replace the tail with a marker so a grep/cut consumer can see
        // the line was capped and the missing bytes are explained.
        buf.truncate(MAX_LINE - TRUNC_TAIL.len());
        buf.push_str(TRUNC_TAIL);
    }
}

fn push_sanitised(out: &mut String, src: &str, also_space: bool) {
    for c in src.chars() {
        let bad = c == '\n' || c == '\t' || (also_space && c == ' ');
        out.push(if bad { '_' } else { c });
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn thread_token() -> usize {
    // On macOS pthread_self() returns a stable, opaque pointer per
    // thread; surfacing the low bits is enough to disambiguate threads
    // in one process's log and costs zero syscalls.
    unsafe { libc::pthread_self() as usize }
}

/// Self-built ISO-8601 in UTC with ms precision. No chrono dep for one
/// format string — Howard Hinnant's days→civil algorithm is public
/// domain (http://howardhinnant.github.io/date_algorithms.html).
fn iso8601_ms(epoch_ms: u64) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let ms = (epoch_ms % 1000) as u32;
    let days = (secs.div_euclid(86400)) as i32;
    let day_secs = secs.rem_euclid(86400) as u32;
    let hr = day_secs / 3600;
    let mi = (day_secs % 3600) / 60;
    let se = day_secs % 60;
    let (y, mo, da) = days_from_epoch_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, da, hr, mi, se, ms
    )
}

fn days_from_epoch_to_ymd(days: i32) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_final = if m <= 2 { y + 1 } else { y };
    (y_final, m, d)
}

// === Macros (`#[macro_export]` lifts them to the crate root) ===

#[macro_export]
#[doc(hidden)]
macro_rules! __lx_internal {
    ($level:expr, $tag:expr, $msg:expr $(, $field_key:ident = $field_val:expr)* $(,)?) => {
        if $crate::logx::should_log($level) {
            $crate::logx::event(
                $level,
                $tag,
                $msg,
                &[ $( (stringify!($field_key), &$field_val as &dyn ::std::fmt::Display) ),* ],
            );
        }
    };
}

#[macro_export]
macro_rules! lx_trace {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Trace, $tag, $msg $(, $($t)*)?);
    };
}
#[macro_export]
macro_rules! lx_debug {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Debug, $tag, $msg $(, $($t)*)?);
    };
}
#[macro_export]
macro_rules! lx_info {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Info, $tag, $msg $(, $($t)*)?);
    };
}
#[macro_export]
macro_rules! lx_warn {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Warn, $tag, $msg $(, $($t)*)?);
    };
}
#[macro_export]
macro_rules! lx_error {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Error, $tag, $msg $(, $($t)*)?);
    };
}
/// Lifecycle event — same as `lx_info!` but takes the sup_log tradition
/// of UPPER_SNAKE_CASE tags so structured greps stand out:
/// `grep $'\tEXECV_' marspot.log`.
#[macro_export]
macro_rules! lx_event {
    ($tag:expr, $msg:expr $(, $($t:tt)*)?) => {
        $crate::__lx_internal!($crate::logx::Level::Info, $tag, $msg $(, $($t)*)?);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_filter_blocks_lower() {
        // Reset the global level for the test — atomics survive across tests.
        GLOBAL_LEVEL.store(Level::Warn as u8, Ordering::Relaxed);
        assert!(!should_log(Level::Info));
        assert!(!should_log(Level::Debug));
        assert!(should_log(Level::Warn));
        assert!(should_log(Level::Error));
        GLOBAL_LEVEL.store(Level::Info as u8, Ordering::Relaxed);
    }

    #[test]
    fn level_parse_accepts_common_aliases() {
        assert_eq!(Level::parse("trace"), Some(Level::Trace));
        assert_eq!(Level::parse("INFO"), Some(Level::Info));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("err"), Some(Level::Error));
        assert_eq!(Level::parse("nope"), None);
    }

    #[test]
    fn iso8601_at_epoch_is_1970() {
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_matches_known_timestamps() {
        // 2024-01-01T00:00:00.000Z = 1704067200 s = 19724 days (54×365 + 13 leap).
        assert_eq!(iso8601_ms(1704067200_000), "2024-01-01T00:00:00.000Z");
        // 2024-02-29 (leap day) at 12:00:00.500Z.
        assert_eq!(
            iso8601_ms(1704067200_000 + 59 * 86400_000 + 12 * 3600_000 + 500),
            "2024-02-29T12:00:00.500Z"
        );
        // 2026-12-31T23:59:59.999Z — multi-year + leap endpoint sanity.
        // Offset = 366 (2024 leap) + 365 (2025) + 364 (Jan 1 → Dec 31 2026) = 1095 days.
        let ts = 1704067200_000_u64 + 1095 * 86400_000 + 23 * 3600_000 + 59 * 60_000 + 59_000 + 999;
        assert_eq!(iso8601_ms(ts), "2026-12-31T23:59:59.999Z");
    }

    #[test]
    fn format_line_under_512() {
        // Force component so the test doesn't depend on order with other tests.
        let _ = COMPONENT.set("test");
        let mut buf = String::new();
        format_line(
            &mut buf,
            Level::Info,
            "session.reader.exit",
            "EOF on pty after 1234 chunks",
            &[("id", &42u32), ("bytes", &1048576u64), ("dur_ms", &"123.5")],
        );
        assert!(buf.len() <= 512, "line was {} bytes", buf.len());
        assert!(buf.ends_with('\n'));
        // pid + tid present
        assert!(buf.contains(&format!("{}", std::process::id())));
        // field rendered
        assert!(buf.contains("id=42"));
        assert!(buf.contains("bytes=1048576"));
    }

    #[test]
    fn format_line_replaces_tabs_and_newlines_in_msg() {
        let _ = COMPONENT.set("test");
        let mut buf = String::new();
        format_line(
            &mut buf,
            Level::Info,
            "tag",
            "first\tsecond\nthird",
            &[],
        );
        // Tabs in msg get replaced so column count stays constant.
        let count = buf.matches('\t').count();
        // Expected separators: ISO/unix/lvl/comp/pid/tid/tag = 7 separators before msg + 0 after for empty fields.
        assert_eq!(count, 7, "unexpected tab count in {:?}", buf);
    }

    #[test]
    fn format_line_truncates_oversize() {
        let _ = COMPONENT.set("test");
        let big_msg = "x".repeat(2048);
        let mut buf = String::new();
        format_line(&mut buf, Level::Info, "tag", &big_msg, &[]);
        assert!(buf.len() <= MAX_LINE, "line was {} bytes", buf.len());
        assert!(buf.ends_with(TRUNC_TAIL));
    }
}
