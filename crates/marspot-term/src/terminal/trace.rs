//! `MARSPOT_TRACE_ESC=<path>`: every control code and control sequence
//! the terminal acts on, one line each, appended to that file.

use std::fs::File;
use std::sync::{Mutex, OnceLock};

/// The trace file, opened on first use; `None` when the variable is
/// not set, which is the only case the dispatch path ever sees in
/// normal use.
fn trace_file() -> Option<&'static Mutex<File>> {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        std::env::var("MARSPOT_TRACE_ESC").ok().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()
                .map(Mutex::new)
        })
    })
    .as_ref()
}

/// Called once per line feed and once per sequence, so with tracing
/// off it has to be a load and a branch, not a call.
#[inline(always)]
pub(super) fn trace_seq(kind: &str, intermediates: &[u8], params: &[u16], byte: u8) {
    if let Some(file) = trace_file() {
        write_line(file, kind, intermediates, params, byte);
    }
}

#[cold]
fn write_line(file: &Mutex<File>, kind: &str, intermediates: &[u8], params: &[u16], byte: u8) {
    use std::io::Write;
    let mut s = String::with_capacity(64);
    s.push_str(kind);
    if !intermediates.is_empty() {
        s.push(' ');
        for &b in intermediates {
            s.push(b as char);
        }
    }
    for (i, p) in params.iter().enumerate() {
        s.push(if i == 0 { ' ' } else { ';' });
        s.push_str(&p.to_string());
    }
    s.push(' ');
    if (0x20..=0x7e).contains(&byte) {
        s.push(byte as char);
    } else {
        s.push_str(&format!("0x{:02x}", byte));
    }
    s.push('\n');
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(s.as_bytes());
    }
}
