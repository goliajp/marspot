//! Web panes in the core: opening one (the sandbox's dev seam for now),
//! taking what its connection thread published, handing its picture to
//! the frame and telling it when that frame has finished.

use std::sync::atomic::{AtomicBool, Ordering};

use marspot::pane::PaneBackend;
use marspot::render_metal::pass_parts::WebImage;
use marspot::web_pane::{Opening, WebPane};
use objc2::Message;

use super::*;

/// The dev seam opens its pane once per process.
static DEV_OPENED: AtomicBool = AtomicBool::new(false);

/// Where the page of a pane in cell `rect` goes: below the title strip,
/// at whole device pixels.
fn page_rect(layout: &Layout, i: usize) -> Option<(f64, f64, f64, f64)> {
    let c = layout.cells.get(i)?;
    let top = c.y_top + layout.cell_title_h;
    let (x, y) = (c.x.ceil(), top.ceil());
    Some((x, y, (c.x + c.w - x).floor(), (c.y_top + c.h - y).floor()))
}

impl CoreApp {
    /// Dev seam (`MARSPOT_DEV_WEB_PANE=<file URL>`): once the first
    /// window is laid out, open one web pane in it on that page.  For
    /// the sandbox only (see `web_pane::dev_url`).
    pub(super) fn dev_drive_web_pane(&mut self) {
        if DEV_OPENED.load(Ordering::Relaxed) || self.windows.is_empty() {
            return;
        }
        let Some(url) = marspot::web_pane::dev_url() else { return };
        let laid_out = !win!(self, 0).panes.is_empty() && !win!(self, 0).layout.cells.is_empty();
        if !laid_out || win!(self, 0).surfaces.is_none() {
            return;
        }
        DEV_OPENED.store(true, Ordering::Relaxed);
        self.open_web_pane(0, url);
    }

    /// Add a web pane for `url` to window `wi`, focused, and open it at
    /// the size the layout gives it.
    pub(super) fn open_web_pane(&mut self, wi: usize, url: String) {
        let id = match allocate_next_session_id() {
            Ok(id) => id,
            Err(e) => return lx_warn!("web_pane.no_id", &e.to_string()),
        };
        let tx = self.event_tx.clone();
        let notify = Box::new(move || {
            let _ = tx.send(CoreEvent::WebPane(id));
        });
        let pane = Pane::new_web(WebPane::new(id, url.clone(), 80, 24, notify));
        let w = &mut win!(self, wi);
        w.panes.push(pane);
        w.focused_idx = w.panes.len() - 1;
        self.rebuild_layout(wi);
        let idx = win!(self, wi).panes.len() - 1;
        let Some((_, _, pw, ph)) = page_rect(&win!(self, wi).layout, idx) else {
            return lx_warn!("web_pane.no_cell", "the layout gave the web pane no cell", session = id);
        };
        let scale = (marspot::layout::density() * 100.0).round() as u32;
        let socket = marspot_term::paths::state_root().join(marspot::web_pane::HOST_SOCKET);
        let device = self.renderer.device().retain();
        lx_event!("WEB_PANE_OPEN", "opening a web pane", session = id, url = &url, width_px = pw, height_px = ph, scale = scale);
        if let PaneBackend::Web(web) = win!(self, wi).panes[idx].session_mut() {
            web.open(socket, Opening { url, px: (pw, ph), scale }, device);
        }
        win!(self, wi).needs_render = true;
    }

    /// The connection thread of web pane `id` published something.
    pub(super) fn web_pane_note(&mut self, id: u64) {
        for w in self.windows.iter_mut() {
            for p in w.panes.iter_mut() {
                if let PaneBackend::Web(web) = p.session_mut()
                    && web.id() == id
                    && web.refresh()
                {
                    lx_info!("web_pane.note", &web.note().to_string(), session = id);
                    w.needs_render = true;
                }
            }
        }
    }

    /// The pictures of window `wi`'s web panes, into the frame about to
    /// be drawn.  Reuses the renderer's list, so a frame allocates
    /// nothing for it.
    pub(super) fn collect_web_images(&mut self, wi: usize, cells: usize) {
        self.renderer.web_images.clear();
        let w = &mut win!(self, wi);
        for (i, p) in w.panes.iter_mut().enumerate().take(cells) {
            let PaneBackend::Web(web) = p.session_mut() else { continue };
            let (Some((x, y, pw, ph)), Some(im)) = (page_rect(&w.layout, i), web.image()) else { continue };
            // a pane made smaller than its page shows the page's top left
            let (dw, dh) = (f64::from(im.width).min(pw).max(0.0), f64::from(im.height).min(ph).max(0.0));
            self.renderer.web_images.push(WebImage {
                texture: im.texture.clone(),
                rect: [x as f32, y as f32, dw as f32, dh as f32],
                uv1: [(dw / f64::from(im.width)) as f32, (dh / f64::from(im.height)) as f32],
            });
        }
    }

    /// Window `wi`'s frame, written to surface `surface`, has finished.
    pub(super) fn web_frames_finished(&mut self, wi: usize, surface: u32) {
        let w = &mut win!(self, wi);
        for (i, p) in w.panes.iter_mut().enumerate() {
            let PaneBackend::Web(web) = p.session_mut() else { continue };
            let Some(seq) = web.frame_finished() else { continue };
            let (Some((x, y, pw, ph)), Some((iw, ih))) = (page_rect(&w.layout, i), web.picture_size()) else { continue };
            lx_event!(
                "WEB_PANE_DRAWN",
                "a frame showing a web pane's page finished",
                session = web.id(),
                seq = seq,
                surface_id = surface,
                x = x,
                y = y,
                width = f64::from(iw).min(pw),
                height = f64::from(ih).min(ph),
                scale = marspot::layout::density()
            );
        }
    }
}
