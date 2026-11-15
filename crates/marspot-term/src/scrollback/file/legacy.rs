//! History written in the format before segments, read where it is.
//!
//! A pane that was running when its session binary was replaced has a
//! hot pair (`scrollback.bin` / `.idx`), maybe a cold pair beside it,
//! and their sidecars (`.marks`, `.clusters`, `.clustertext`).  They
//! are opened read-only, as the oldest history, and nothing is written
//! to them: a binary rolled back to finds them as it left them.  The
//! budget deletes them once two full segments are in use, before it
//! lets go of anything newer.

use super::*;
use crate::scrollback::sidecar::LineExtras;

pub(super) struct Legacy {
    pub(super) bin: std::fs::File,
    pub(super) idx: std::fs::File,
    pub(super) bin_path: std::path::PathBuf,
    pub(super) idx_path: std::path::PathBuf,
    pub(super) first_line: u64,
    pub(super) lines: u64,
    pub(super) extras: LineExtras,
}

impl Legacy {
    /// The pair at `bin_path` / `idx_path`, or `None` when there is
    /// none.  A pair that is not one this build can read is an error,
    /// for the caller to set aside.
    pub(super) fn open(bin_path: &std::path::Path, idx_path: &std::path::Path) -> std::io::Result<Option<Self>> {
        let bin = match std::fs::File::open(bin_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let bin_len = bin.metadata()?.len();
        let idx = match std::fs::File::open(idx_path) {
            Ok(f) => f,
            // a pair that never got its index has no lines anyone can find
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && bin_len <= FILE_HEADER_BYTES => return Ok(None),
            Err(e) => return Err(e),
        };
        let invalid = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
        if bin_len < FILE_HEADER_BYTES {
            return Err(invalid(format!("scrollback bin {bin_len} bytes < {FILE_HEADER_BYTES} header")));
        }
        let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
        read_exact_at(&bin, &mut hdr, 0)?;
        let magic = u32::from_le_bytes(hdr[0..4].try_into().expect("four bytes"));
        if magic != FILE_MAGIC {
            return Err(invalid(format!("scrollback bin magic 0x{magic:08x} != 0x{FILE_MAGIC:08x}")));
        }
        let version = u32::from_le_bytes(hdr[4..8].try_into().expect("four bytes"));
        if !(FILE_MIN_COMPAT..=FILE_VERSION).contains(&version) {
            return Err(invalid(format!("scrollback bin version {version} outside [{FILE_MIN_COMPAT}, {FILE_VERSION}]")));
        }
        let cell_abi = u32::from_le_bytes(hdr[8..12].try_into().expect("four bytes"));
        let expected = if version == 2 { V2_CELL_BYTES } else { crate::grid::CELL_MEM_BYTES };
        if cell_abi != expected as u32 && cell_abi != crate::grid::CELL_MEM_BYTES as u32 {
            return Err(invalid(format!("scrollback bin v{version} cell_abi {cell_abi} != {expected}")));
        }
        let idx_len = idx.metadata()?.len();
        if idx_len % 8 != 0 {
            return Err(invalid(format!("scrollback idx size {idx_len} not a multiple of 8")));
        }
        // The lines are the index entries whose record is whole in the
        // file: a process killed with the index ahead of the records
        // leaves a tail that is not.  Counted, not cut -- nothing here
        // is written.
        let whole = |i: u64| {
            read_idx_at(&idx, i).is_ok_and(|off| {
                let mut len = [0u8; 4];
                off >= FILE_HEADER_BYTES
                    && read_exact_at(&bin, &mut len, off).is_ok()
                    && u32::from_le_bytes(len) >= 3
                    && off + 4 + u64::from(u32::from_le_bytes(len)) <= bin_len
            })
        };
        let mut lines = idx_len / 8;
        while lines > 0 && !whole(lines - 1) {
            lines -= 1;
        }
        let extras = match u64::from_le_bytes(hdr[24..32].try_into().expect("eight bytes")) {
            // written before epochs: nothing was filed against it
            0 => LineExtras::none(),
            epoch => LineExtras::open_cold_for_read(bin_path, epoch),
        };
        Ok(Some(Self {
            bin,
            idx,
            bin_path: bin_path.to_path_buf(),
            idx_path: idx_path.to_path_buf(),
            first_line: 0,
            lines,
            extras,
        }))
    }

    /// The files to delete when the budget passes this pair.
    pub(super) fn paths(&self) -> Vec<std::path::PathBuf> {
        let mut out = vec![self.bin_path.clone(), self.idx_path.clone()];
        out.extend(LineExtras::paths_beside(&self.bin_path));
        out
    }
}

/// The pairs of the format before segments beside `bin_path`: the cold
/// one, then the hot one.
pub(super) fn open_all(bin_path: &std::path::Path, idx_path: &std::path::Path) -> std::io::Result<Vec<Legacy>> {
    let cold = Legacy::open(&bin_path.with_extension("cold.bin"), &idx_path.with_extension("cold.idx"))
        .ok()
        .flatten();
    let hot = Legacy::open(bin_path, idx_path)?;
    let mut out: Vec<Legacy> = cold.into_iter().chain(hot).collect();
    let mut first = 0;
    for l in &mut out {
        l.first_line = first;
        first += l.lines;
    }
    Ok(out)
}

/// Every file of the format before segments beside `bin_path`.
pub(super) fn all_paths(bin_path: &std::path::Path, idx_path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let (cold_bin, cold_idx) = (bin_path.with_extension("cold.bin"), idx_path.with_extension("cold.idx"));
    let mut out = vec![bin_path.to_path_buf(), idx_path.to_path_buf(), cold_bin.clone(), cold_idx];
    out.extend(LineExtras::paths_beside(bin_path));
    out.extend(LineExtras::paths_beside(&cold_bin));
    out
}
