//! Watch the pixels marspot actually presents, and report black frames.
//!
//! Everything else in this repo measures the grid — what the terminal
//! *means* to show.  A flash lives further down: L2 draws into an
//! IOSurface, L1 hands that surface to the compositor, and a frame
//! nobody meant to show can still reach the glass.  The surfaces are
//! reachable by id from any process (that is the point of them), so
//! this reads the same bytes the display does.
//!
//!   cargo run --release --example surface_watch -- <log-path> [ms]
//!
//! The pair is re-read from the log's most recent
//! `shell.presenter.pair_swapped` line, and re-read as the watch runs:
//! a window that re-attaches gets a NEW pair, and a watch pinned to
//! the old one goes quiet and reports nothing — which is
//! indistinguishable from "nothing went wrong".
//!
//! ## Which surface is on screen
//!
//! Two are mapped: one being shown, one being drawn into.  Sampling
//! both and taking the darker would call every half-drawn back buffer
//! a flash.  So a surface counts only once it is SETTLED — its content
//! identical across two consecutive polls — and the displayed one is
//! whichever settled most recently.  A buffer under active drawing
//! changes every poll and is never mistaken for the screen.
use std::ffi::c_void;

type IOSurfaceRef = *const c_void;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLookup(csid: u32) -> IOSurfaceRef;
    fn IOSurfaceGetWidth(b: IOSurfaceRef) -> usize;
    fn IOSurfaceGetHeight(b: IOSurfaceRef) -> usize;
    fn IOSurfaceGetBytesPerRow(b: IOSurfaceRef) -> usize;
    fn IOSurfaceGetBaseAddress(b: IOSurfaceRef) -> *mut c_void;
    fn IOSurfaceLock(b: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(b: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
}
/// `kIOSurfaceLockReadOnly` — never write to a surface we do not own.
const LOCK_READ_ONLY: u32 = 1;

struct Surface {
    raw: IOSurfaceRef,
    id: u32,
}

/// One reading of a surface: how bright it is, and a hash to tell
/// "still being drawn" from "finished".
struct Sample {
    lit: f32,
    hash: u64,
}

impl Surface {
    fn lookup(id: u32) -> Option<Self> {
        let raw = unsafe { IOSurfaceLookup(id) };
        (!raw.is_null()).then_some(Self { raw, id })
    }

    /// Fraction of pixels that are not the background, plus a hash.
    ///
    /// Sampled on a coarse lattice: a flash is a whole-surface event,
    /// and reading every pixel of a 4K surface every 2 ms would be
    /// measuring the prober.
    fn sample(&self) -> Option<Sample> {
        let (w, h) = unsafe {
            (IOSurfaceGetWidth(self.raw), IOSurfaceGetHeight(self.raw))
        };
        if w == 0 || h == 0 {
            return None;
        }
        let mut seed = 0u32;
        if unsafe { IOSurfaceLock(self.raw, LOCK_READ_ONLY, &mut seed) } != 0 {
            return None;
        }
        let base = unsafe { IOSurfaceGetBaseAddress(self.raw) } as *const u8;
        let stride = unsafe { IOSurfaceGetBytesPerRow(self.raw) };
        let (mut lit, mut total, mut hash) = (0u64, 0u64, 0xcbf29ce484222325u64);
        // ~200x120 lattice, whatever the surface size.
        let (sx, sy) = ((w / 200).max(1), (h / 120).max(1));
        let mut y = 0;
        while y < h {
            let row = unsafe { base.add(y * stride) };
            let mut x = 0;
            while x < w {
                let px = unsafe { std::slice::from_raw_parts(row.add(x * 4), 4) };
                // BGRA8.  "Lit" = brighter than the darkest chrome; the
                // terminal's own background is near-black, so this
                // counts text and UI rather than absolute luminance.
                let v = px[0].max(px[1]).max(px[2]);
                if v > 60 {
                    lit += 1;
                }
                total += 1;
                hash ^= v as u64;
                hash = hash.wrapping_mul(0x100000001b3);
                x += sx;
            }
            y += sy;
        }
        unsafe { IOSurfaceUnlock(self.raw, LOCK_READ_ONLY, &mut seed) };
        Some(Sample { lit: lit as f32 / total.max(1) as f32, hash })
    }
}

/// The pair the presenter is currently showing, per the log.
fn current_pair(log: &str) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(log).ok()?;
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("presenter.pair_swapped"))?;
    let grab = |key: &str| -> Option<u32> {
        let i = line.find(key)? + key.len();
        line[i..]
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    Some((grab("front=")?, grab("back=")?))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.is_empty() {
        eprintln!("usage: surface_watch <log-path> [ms]");
        std::process::exit(2);
    }
    let log = a[0].clone();
    let ms: u64 = a.get(1).map(|s| s.parse().unwrap()).unwrap_or(2500);
    let Some(pair) = current_pair(&log) else {
        eprintln!("no presenter.pair_swapped line in {log}");
        std::process::exit(1);
    };
    let mut ids = pair;
    let mut surfaces: Vec<Surface> = [ids.0, ids.1]
        .iter()
        .filter_map(|id| Surface::lookup(*id))
        .collect();
    if surfaces.len() != 2 {
        eprintln!("could not look up both surfaces of {ids:?}");
        std::process::exit(1);
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    // A frame counts the moment a surface stops changing after having
    // changed — that transition IS "the GPU finished this one, it is
    // what the display shows now".  A surface nobody has drawn into
    // never changes, so it never qualifies; without that rule the
    // spare buffer, which is legitimately black, reads as a flash on
    // every poll.
    let mut prev: Vec<Option<u64>> = vec![None; 2];
    let mut dirty: Vec<bool> = vec![false; 2];
    let mut shown: Vec<(u32, f32)> = Vec::new();
    let mut polls = 0u64;
    while std::time::Instant::now() < deadline {
        for (i, s) in surfaces.iter().enumerate() {
            let Some(cur) = s.sample() else { continue };
            match prev[i] {
                Some(h) if h == cur.hash => {
                    if dirty[i] {
                        dirty[i] = false;
                        shown.push((s.id, cur.lit));
                    }
                }
                Some(_) => dirty[i] = true,
                // First sighting is not a frame: we did not watch it
                // arrive, so we cannot say it was ever displayed.
                None => {}
            }
            prev[i] = Some(cur.hash);
        }
        polls += 1;
        // Follow a re-attach.  Cheap enough at 20 Hz, and the
        // alternative is a watch that silently observes a dead pair.
        if polls % 10 == 0 {
            if let Some(now) = current_pair(&log) {
                if now != ids {
                    let fresh: Vec<Surface> = [now.0, now.1]
                        .iter()
                        .filter_map(|id| Surface::lookup(*id))
                        .collect();
                    if fresh.len() == 2 {
                        println!("  (pair changed {ids:?} -> {now:?})");
                        ids = now;
                        surfaces = fresh;
                        prev = vec![None; 2];
                        dirty = vec![false; 2];
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // A flash is not "a dark frame" — a screen showing four short
    // lines is legitimately dark, and calling that a fault is the same
    // mistake as measuring a grid's fill and forgetting the content
    // was narrow.  A flash is a frame far darker than BOTH the frame
    // before and the frame after: something vanished and came back.
    let mut dips: Vec<(usize, f32, f32)> = Vec::new();
    for i in 1..shown.len().saturating_sub(1) {
        let (before, here, after) = (shown[i - 1].1, shown[i].1, shown[i + 1].1);
        let neighbour = before.min(after);
        if here < neighbour * 0.25 && neighbour - here > 0.02 {
            dips.push((i, neighbour, here));
        }
    }
    println!("{} frames reached the display over {ms}ms ({polls} polls)", shown.len());
    let seq: Vec<String> = shown.iter().map(|(_, l)| format!("{:.1}%", l * 100.0)).collect();
    let shown_seq = if seq.len() > 24 {
        format!("{} … {}", seq[..12].join(" -> "), seq[seq.len() - 12..].join(" -> "))
    } else {
        seq.join(" -> ")
    };
    println!("  lit sequence: {shown_seq}");
    if dips.is_empty() {
        println!("PASS: no frame went dark between two lit ones");
        return;
    }
    for (i, neighbour, here) in &dips {
        println!(
            "  frame {i}: {:.1}% between neighbours of {:.1}% — a flash",
            here * 100.0,
            neighbour * 100.0
        );
    }
    println!("FLASH: {} dark frame(s) reached the display", dips.len());
    std::process::exit(1);
}
