//! A web pane: a page that madcawl's host opens in a content process of
//! its own and that draws into shared memory this process shows as a
//! texture in the pane's place.  The pane on the main thread holds a
//! small grid for what it has to say (opening, a version that does not
//! match, a failure) and the latest texture its connection thread
//! published; the thread does all the talking (`link`).

mod frames;
mod dev;
pub mod dev_script;
pub mod gpu;
pub mod input;
mod link;
mod link_input;

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use madcawl_wire::Range;

use crate::grid::Grid;
use crate::ui::strings::{self, Arg};
pub use dev::{dev_host_program, dev_url, entry_points};
pub use frames::Unspoken;
use gpu::{Device, Texture};

/// The socket the shell binds for madcawl's host, under the state root.
pub const HOST_SOCKET: &str = "madcawl.sock";

/// What the pane says in place of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    Opening,
    Shown,
    /// Which side, our range, and how the ranges failed to meet
    Unspoken(&'static str, Range, Unspoken),
    Failed(String),
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// A picture to draw: the texture and its size in device pixels.
pub struct Image<'a> {
    pub texture: &'a Texture,
    pub width: u32,
    pub height: u32,
}

struct Shown {
    seq: u64,
    texture: Texture,
    size: (u32, u32),
}

#[derive(Default)]
struct Inner {
    note: Option<Note>,
    shown: Option<Shown>,
    /// The newest commit a finished frame showed.
    drawn: u64,
    /// Where the page shown is scrolled to, CSS px
    scrolled: (u32, u32),
    out: std::collections::VecDeque<input::Out>,
    said: input::Said,
}

/// What the connection thread and the pane share.
pub struct Shared {
    inner: Mutex<Inner>,
    notify: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_note(&self, note: Note) {
        self.lock().note = Some(note);
        (self.notify)();
    }

    fn show(&self, seq: u64, texture: Texture, size: (u32, u32), scrolled: (u32, u32)) {
        {
            let mut g = self.lock();
            g.shown = Some(Shown { seq, texture, size });
            g.scrolled = scrolled;
            g.note = Some(Note::Shown);
        }
        (self.notify)();
    }

    fn drawn(&self) -> u64 {
        self.lock().drawn
    }
}

/// Where and how a pane's page is opened.
pub struct Opening {
    pub url: String,
    /// The pane, device pixels
    pub px: (f64, f64),
    /// Device pixels per CSS px, in hundredths
    pub scale: u32,
}

pub struct WebPane {
    id: u64,
    grid: Grid,
    note: Note,
    url: String,
    shared: Arc<Shared>,
    /// The write end of the thread's wake pipe; dropping it ends the thread.
    wake: Option<OwnedFd>,
    /// The texture last handed to a frame, and the commit it shows.
    current: Option<(u64, Texture, (u32, u32))>,
    /// The commit a frame showed that has not finished yet, and the last
    /// one the thread was told of.
    in_flight: u64,
    told: u64,
    /// Larger each time the grid or the picture changes.
    rev: u64,
    visible: bool,
    /// The pane's size in device pixels and its scale, once opened
    px: Option<((f64, f64), u32)>,
    /// The page took a press that has not been let go of
    pub pressed: bool,
    /// The pointer is over the page
    pub hovered: bool,
}

impl WebPane {
    /// A pane for `url` that has not opened it yet; `notify` is called
    /// on the connection thread whenever there is something new to draw.
    pub fn new(id: u64, url: String, cols: u16, rows: u16, notify: Box<dyn Fn() + Send + Sync>) -> WebPane {
        let shared = Arc::new(Shared { inner: Mutex::new(Inner::default()), notify });
        let mut p = WebPane {
            id,
            grid: Grid::new(cols.max(20), rows.max(3)),
            note: Note::Opening,
            url,
            shared,
            wake: None,
            current: None,
            in_flight: 0,
            told: 0,
            rev: 0,
            visible: true,
            px: None,
            pressed: false,
            hovered: false,
        };
        p.paint();
        p
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn note(&self) -> &Note {
        &self.note
    }

    /// Start the connection thread for a pane of `o.px` device pixels.
    pub fn open(&mut self, socket: PathBuf, o: Opening, device: Device) {
        if self.wake.is_some() {
            return;
        }
        let Some((css, px)) = frames::page_size(o.px.0, o.px.1, o.scale) else {
            return self.say(Note::Failed(format!("the pane is too small: {:?}", o.px)));
        };
        let Some((read, write)) = link_input::pipe() else {
            return self.say(Note::Failed("no wake pipe".into()));
        };
        self.px = Some((o.px, o.scale));
        let open = link::Open { socket, session: self.id, url: o.url, css, px, scale: o.scale };
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("web-pane-{}", self.id))
            .spawn(move || link::run(open, device, shared, read));
        match spawned {
            Ok(_) => self.wake = Some(write),
            Err(e) => self.say(Note::Failed(format!("no connection thread: {e}"))),
        }
    }

    fn say(&mut self, note: Note) {
        self.note = note;
        self.paint();
    }

    /// Take what the thread published; whether the pane looks different.
    pub fn refresh(&mut self) -> bool {
        let (note, shown) = {
            let g = self.shared.lock();
            (g.note.clone(), g.shown.as_ref().map(|s| (s.seq, s.texture.clone(), s.size)))
        };
        let mut changed = false;
        if let Some(note) = note.filter(|n| *n != self.note) {
            self.say(note);
            changed = true;
        }
        if shown.as_ref().map(|s| s.0) != self.current.as_ref().map(|c| c.0) {
            self.current = shown;
            self.rev += 1;
            changed = true;
        }
        changed
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Where the page shown is scrolled to, CSS px.
    pub fn scrolled(&self) -> (u32, u32) {
        self.shared.lock().scrolled
    }

    /// The size of the picture shown, device pixels.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.current.as_ref().map(|c| c.2)
    }

    /// The picture a frame being built draws, noted as the one it shows.
    pub fn image(&mut self) -> Option<Image<'_>> {
        let (seq, texture, (width, height)) = self.current.as_ref()?;
        self.in_flight = *seq;
        Some(Image { texture, width: *width, height: *height })
    }

    /// The frame that drew `image` has finished: the thread may hand
    /// older buffers back.  Whether one was drawn that it had not heard of.
    pub fn frame_finished(&mut self) -> Option<u64> {
        if self.in_flight <= self.told {
            return None;
        }
        self.told = self.in_flight;
        self.shared.lock().drawn = self.told;
        self.wake();
        Some(self.told)
    }

    fn wake(&self) {
        if let Some(w) = &self.wake {
            // the pipe does not block; a full one already wakes the thread
            // SAFETY: one byte from a live buffer to our own pipe
            unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(w), [1u8].as_ptr().cast(), 1) };
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid = Grid::new(cols.max(20), rows.max(3));
        self.paint();
    }

    fn paint(&mut self) {
        self.rev += 1;
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        self.grid = Grid::new(cols, rows);
        let text = match &self.note {
            // the page covers the pane; text under it would show through
            Note::Shown => return,
            Note::Opening => strings::text(Arg::WebOpening(&self.url)),
            Note::Unspoken(side, ours, u) => {
                let theirs = match u {
                    Unspoken::TooOld(Some(r)) | Unspoken::TooNew(r) => Some((r.min, r.cur)),
                    Unspoken::TooOld(None) | Unspoken::Bad(_) => None,
                };
                strings::text(Arg::WebVersion { side, theirs, ours: (ours.min, ours.cur) })
            }
            Note::Failed(why) => strings::text(Arg::WebFailed(why)),
        };
        let row = 1u16.min(rows.saturating_sub(1));
        crate::pane_message::put(&mut self.grid, 2, row, &text);
    }
}
