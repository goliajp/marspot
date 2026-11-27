//! The core's toolbar icons, drawn for it.
//!
//! Drawing a system symbol registers the drawing process with
//! LaunchServices, and the core must not be registered, so it asks this
//! process.  This process does not draw them either: loading the symbol
//! set costs about a megabyte that stays for the life of the process,
//! the resident shell's included.  A short-lived core run with
//! `--draw-icons` draws a batch and exits; what it drew is kept here and
//! served to every core after.
//!
//! The drawer is started only from the event loop, so this process has
//! registered and holds the coalition: the drawer's registration finds
//! the coalition taken and its dead record holds none.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use marspot::app::EventProxy;
use marspot::{lx_event, lx_warn};

/// `(symbol, point_px, side, mask)`.
pub(crate) type Drawn = (u16, u16, u16, Vec<u8>);

/// Seven symbols at the few sizes the chrome uses; a bound all the
/// same, oldest out first.
const KEPT_MOST: usize = 64;

pub(crate) struct CoreIcons {
    kept: VecDeque<Drawn>,
    waiting: Vec<(u16, u16)>,
    drawing: bool,
    tx: Sender<Vec<Drawn>>,
    rx: Receiver<Vec<Drawn>>,
}

impl CoreIcons {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self { kept: VecDeque::new(), waiting: Vec::new(), drawing: false, tx, rx }
    }

    /// The core wants `symbol` at `point_px`: the mask when it is kept,
    /// otherwise `None`, and it is waiting to be drawn.
    pub(crate) fn want(&mut self, symbol: u16, point_px: u16) -> Option<Drawn> {
        if let Some(d) = self.kept.iter().find(|d| (d.0, d.1) == (symbol, point_px)) {
            return Some(d.clone());
        }
        if !self.waiting.contains(&(symbol, point_px)) {
            self.waiting.push((symbol, point_px));
        }
        None
    }

    /// Start a drawer for what is waiting, unless one is at work.
    pub(crate) fn draw_waiting(&mut self, drawer: PathBuf, proxy: &EventProxy) {
        if self.drawing || self.waiting.is_empty() {
            return;
        }
        self.drawing = true;
        let batch = std::mem::take(&mut self.waiting);
        let (tx, proxy) = (self.tx.clone(), proxy.clone());
        std::thread::spawn(move || {
            let _ = tx.send(run_drawer(&drawer, &batch));
            proxy.wake();
        });
    }

    /// What a drawer finished, now kept; the caller sends each to the
    /// core.
    pub(crate) fn drawn(&mut self) -> Vec<Drawn> {
        let mut out = Vec::new();
        while let Ok(batch) = self.rx.try_recv() {
            self.drawing = false;
            for d in batch {
                self.kept.retain(|k| (k.0, k.1) != (d.0, d.1));
                if self.kept.len() >= KEPT_MOST {
                    self.kept.pop_front();
                }
                self.kept.push_back(d.clone());
                out.push(d);
            }
        }
        out
    }
}

/// The masks of `batch`: those in the file of recorded masks a test or
/// the sandbox names, and the rest from a drawer.  Each drawer is one
/// more app registration, and the runner starts the app hundreds of
/// times a day.
fn run_drawer(drawer: &std::path::Path, batch: &[(u16, u16)]) -> Vec<Drawn> {
    use marspot::ui::system::macos::icons::RECORDED_ENV;
    let recorded = std::env::var_os(RECORDED_ENV).and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    let (mut drawn, rest) = from_recorded(&recorded, batch);
    if !rest.is_empty() {
        drawn.extend(draw_with(drawer, &rest));
    }
    drawn
}

/// `(found, not found)` for `batch` in a file of recorded masks.
fn from_recorded(file: &[u8], batch: &[(u16, u16)]) -> (Vec<Drawn>, Vec<(u16, u16)>) {
    let mut found = Vec::new();
    let mut rest = Vec::new();
    for &(symbol, point_px) in batch {
        match marspot::ui::system::macos::icons::find_recorded(file, symbol, point_px) {
            Some((side, mask)) => found.push((symbol, point_px, side, mask.to_vec())),
            None => rest.push((symbol, point_px)),
        }
    }
    (found, rest)
}

/// Run `drawer --draw-icons` for `batch` and read what it wrote: each
/// mask as a length and an `IconMask` payload.
fn draw_with(drawer: &std::path::Path, batch: &[(u16, u16)]) -> Vec<Drawn> {
    let list: Vec<String> = batch.iter().map(|(s, p)| format!("{s}:{p}")).collect();
    let started = std::time::Instant::now();
    let out = match std::process::Command::new(drawer)
        .arg(DRAW_ARG)
        .arg(list.join(","))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            lx_warn!("shell.icons.drawer_failed", "the icon drawer did not finish", status = format!("{:?}", out.status.code()));
            return Vec::new();
        }
        Err(e) => {
            lx_warn!("shell.icons.drawer_failed", &format!("{e}"));
            return Vec::new();
        }
    };
    let drawn = read_masks(&out);
    lx_event!(
        "SHELL_ICONS_DRAWN",
        "a drawer drew the core's icons",
        asked = batch.len(),
        drawn = drawn.len(),
        ms = started.elapsed().as_millis() as u64
    );
    drawn
}

/// The argument a core is run with to draw icons and exit.
pub(crate) const DRAW_ARG: &str = "--draw-icons";

/// The masks in a drawer's output; one that does not decode ends the
/// reading.
fn read_masks(mut out: &[u8]) -> Vec<Drawn> {
    let mut drawn = Vec::new();
    loop {
        let mut len = [0u8; 4];
        if out.read_exact(&mut len).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len) as usize;
        if out.len() < len {
            break;
        }
        let (payload, rest) = out.split_at(len);
        match marspot::shell_proto::decode_icon_mask(payload) {
            Ok(d) => drawn.push(d),
            Err(_) => break,
        }
        out = rest;
    }
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(d: &Drawn) -> Vec<u8> {
        let p = marspot::shell_proto::encode_icon_mask(d.0, d.1, d.2, &d.3);
        let mut v = (p.len() as u32).to_le_bytes().to_vec();
        v.extend(p);
        v
    }

    #[test]
    fn a_drawers_output_is_read_mask_by_mask_and_a_torn_one_ends_it() {
        let a: Drawn = (1, 13, 2, vec![1, 2, 3, 4]);
        let b: Drawn = (2, 26, 1, vec![9]);
        let mut out = framed(&a);
        out.extend(framed(&b));
        assert_eq!(read_masks(&out), vec![a.clone(), b]);
        out.truncate(out.len() - 1);
        assert_eq!(read_masks(&out), vec![a]);
        assert!(read_masks(&[]).is_empty());
    }

    #[test]
    fn recorded_masks_are_served_and_the_rest_left_to_a_drawer() {
        let file = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/appkit/icon-masks.bin")).unwrap();
        let (found, rest) = from_recorded(&file, &[(5, 26), (5, 99)]);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0, found[0].1, found[0].2), (5, 26, 46));
        assert!(found[0].3.iter().any(|&a| a > 0), "a recorded mask has ink");
        assert_eq!(rest, vec![(5, 99)]);
        assert_eq!(from_recorded(&file[..10], &[(0, 13)]), (Vec::new(), vec![(0, 13)]), "a torn file serves nothing");
    }

    #[test]
    fn a_mask_is_asked_for_once_and_served_once_kept() {
        let mut icons = CoreIcons::new();
        assert_eq!(icons.want(3, 13), None);
        assert_eq!(icons.want(3, 13), None);
        assert_eq!(icons.waiting, vec![(3, 13)]);
        icons.tx.send(vec![(3, 13, 1, vec![7])]).unwrap();
        assert_eq!(icons.drawn(), vec![(3, 13, 1, vec![7])]);
        assert_eq!(icons.want(3, 13), Some((3, 13, 1, vec![7])));
    }
}
