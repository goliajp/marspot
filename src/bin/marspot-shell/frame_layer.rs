//! What a window shows: the surface the core last finished, as the
//! contents of a layer.
//!
//! The shell draws nothing.  The window's layer carries a layer whose
//! contents are the IOSurface itself and the window server composites
//! it; a new frame is one property set.  A process that has the GPU
//! execute anything is charged a driver pool of a hundred megabytes and
//! more for as long as it keeps doing so, and the core already pays
//! that once.
//!
//! The window server reads the surface on the window whenever it
//! composites, so the core must not paint a surface until it has left
//! the screen.  The shell says when (`FrameShown`): at the display's
//! refresh after the swap, which is also what holds the core to one
//! frame a refresh.  The refresh comes from a display link that runs
//! only from a swap to that report; the system does not run it for a
//! window nobody can see, so the report, and with it the core's next
//! frame, waits until the window can be seen again.
//!
//! A core that does not know `FramePace` cannot be held off a surface
//! on the screen, so its frames are copied the old way: `legacy` puts a
//! `CAMetalLayer` over the frame layer, made only when such a core is
//! met and dropped at the first frame of one that paces.

use std::cell::{Cell, RefCell};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use core_foundation::base::TCFType;
use core_graphics::color::CGColor;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass};
use objc2_app_kit::{NSColor, NSView, NSViewLayerContentsPlacement};
use objc2_foundation::{NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};
use objc2_quartz_core::{kCAFilterNearest, CAAutoresizingMask, CADisplayLink, CALayer, CATransaction};

use marspot::iosurface::IOSurface;
use marspot::latency_trace::{self as trace, Stage};
use marspot::shell_proto::{encode_frame_shown, Frame, MsgType};

use crate::banner::BannerKind;
use crate::present::{OpaqueCGColor, ShellPresenter};

thread_local! {
    /// The active core's socket, for the reports the display links
    /// send.  Written only on the main thread, like every other frame
    /// to the core.
    static CORE: RefCell<Option<UnixStream>> = const { RefCell::new(None) };
}

/// Reports go to this core from now on; `None` when there is none.
pub(crate) fn set_core(stream: Option<UnixStream>) {
    CORE.with(|c| *c.borrow_mut() = stream);
}

/// What a window's display link needs at a refresh.
struct Owed {
    window_id: u32,
    /// The surface put on the window and not yet reported.
    shown: Cell<Option<u32>>,
    /// The surface it replaced, until the window server lets go of it.
    displaced: RefCell<Option<IOSurface>>,
    /// Refreshes since the swap.
    refreshes: Cell<u8>,
    link: RefCell<Option<Retained<CADisplayLink>>>,
}

impl Owed {
    fn refresh(&self) {
        let pause = || {
            if let Some(l) = self.link.borrow().as_ref() {
                l.setPaused(true);
            }
        };
        let Some(id) = self.shown.get() else { return pause() };
        let n = self.refreshes.get() + 1;
        self.refreshes.set(n);
        // the window server drops the old surface within a millisecond of
        // the swap; a refresh that lands inside that waits for the next
        if n < 2 && self.displaced.borrow().as_ref().is_some_and(IOSurface::in_use) {
            return;
        }
        self.shown.set(None);
        self.displaced.borrow_mut().take();
        pause();
        CORE.with(|c| {
            if let Some(stream) = c.borrow_mut().as_mut() {
                // a core that is gone is the supervisor's to find
                let _ = Frame::new(MsgType::FrameShown, encode_frame_shown(self.window_id, id)).write_to(stream);
                trace::mark(Stage::ShownSent, 0);
            }
        });
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The ivar is used on the main thread only: the link is on the
    //   main run loop.
    #[unsafe(super(NSObject))]
    #[name = "MarspotFrameLink"]
    #[ivars = Rc<Owed>]
    struct LinkTarget;

    impl LinkTarget {
        #[unsafe(method(refresh:))]
        fn refresh(&self, _link: &CADisplayLink) {
            self.ivars().refresh();
        }
    }
);

fn terminal_bg() -> (f64, f64, f64) {
    marspot_term::palette::current().bg
}

fn set_background(layer: &CALayer) {
    let (r, g, b) = terminal_bg();
    let cgcolor = CGColor::rgb(r, g, b, 1.0);
    let cgcolor_ref = cgcolor.as_concrete_TypeRef() as *const OpaqueCGColor;
    let layer_ptr: *const CALayer = layer;
    // SAFETY: `setBackgroundColor:` takes a CGColorRef, live for the call.
    let _: () = unsafe { msg_send![layer_ptr, setBackgroundColor: cgcolor_ref] };
}

/// Run `f` as one change with no implicit animation.
fn unanimated(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}

pub(crate) struct FrameLayers {
    /// The view's own layer: the terminal's background, so the band a
    /// growing window uncovers before the core has drawn at the new
    /// size is the colour the terminal would have painted there.
    root: Retained<CALayer>,
    /// Shows the surface at its own size in pixels, from the top left.
    frame: Retained<CALayer>,
    banner: Option<Retained<CALayer>>,
    /// The copying presenter, for a core that cannot be paced.
    legacy: Option<ShellPresenter>,
    on_window: Option<IOSurface>,
    scale: f64,
    owed: Rc<Owed>,
    /// For comparing the window with what was last shown in it.
    #[cfg(feature = "latency-trace")]
    check: (Retained<NSView>, Option<std::time::Instant>, u64),
}

impl FrameLayers {
    pub(crate) fn new(view: &NSView, window_id: u32, scale: f64) -> Self {
        let root = CALayer::new();
        // the view is flipped: sublayers are placed from the top left
        root.setGeometryFlipped(true);
        set_background(&root);
        let frame = CALayer::new();
        // the core paints every pixel and leaves alpha at one
        frame.setOpaque(true);
        // SAFETY: QuartzCore's own constant strings.
        unsafe {
            frame.setMagnificationFilter(kCAFilterNearest);
            frame.setMinificationFilter(kCAFilterNearest);
        }
        root.addSublayer(&frame);
        view.setWantsLayer(true);
        // AppKit's own placement of the layer during a live resize:
        // anchored top left, never stretched into the new bounds.
        view.setLayerContentsPlacement(NSViewLayerContentsPlacement::TopLeft);
        view.setLayer(Some(&root));
        if let Some(window) = view.window() {
            let (r, g, b) = terminal_bg();
            window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)));
        }

        let owed = Rc::new(Owed {
            window_id,
            shown: Cell::new(None),
            displaced: RefCell::new(None),
            refreshes: Cell::new(0),
            link: RefCell::new(None),
        });
        let target = LinkTarget::alloc().set_ivars(Rc::clone(&owed));
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        let target: Retained<LinkTarget> = unsafe { msg_send![super(target), init] };
        // SAFETY: the target answers the selector, which takes the link;
        // the link retains its target until it is invalidated in `drop`.
        let link = unsafe { view.displayLinkWithTarget_selector(&target, sel!(refresh:)) };
        link.setPaused(true);
        // SAFETY: the main run loop, in the modes a live resize and a
        // menu being tracked also run.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        *owed.link.borrow_mut() = Some(link);

        Self {
            root,
            frame,
            banner: None,
            legacy: None,
            on_window: None,
            scale,
            owed,
            #[cfg(feature = "latency-trace")]
            check: (objc2::Message::retain(view), None, 0),
        }
    }

    /// Whether `layer` is this window's.
    pub(crate) fn is(&self, layer: &CALayer) -> bool {
        std::ptr::eq(Retained::as_ptr(&self.root), layer)
    }

    /// Follow the theme in force.
    pub(crate) fn follow_theme(&self) {
        unanimated(|| {
            set_background(&self.root);
            if let Some(p) = self.legacy.as_ref() {
                crate::present::set_layer_background(&p.layer);
            }
        });
    }

    /// Put `surface` on the window, and owe the core the word that it
    /// is there.
    pub(crate) fn show(&mut self, surface: &IOSurface) {
        trace::mark(Stage::PresentBegin, 0);
        let again = self.on_window.as_ref().is_some_and(|s| s.id() == surface.id());
        let raw: *const AnyObject = surface.raw().cast();
        unanimated(|| {
            // the same surface painted again is not a change the layer
            // passes on; taking it off first makes it one
            if again {
                // SAFETY: no contents.
                unsafe { self.frame.setContents(None) };
            }
            // SAFETY: an IOSurface is an object a layer takes as
            // contents, and the layer retains it.
            unsafe { self.frame.setContents(Some(&*raw)) };
            self.frame.setFrame(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(surface.width() as f64 / self.scale, surface.height() as f64 / self.scale),
            ));
            // a pacing core's first frame retires the copy
            if let Some(p) = self.legacy.take() {
                p.layer.removeFromSuperlayer();
            }
        });
        CATransaction::flush();
        let displaced = self.on_window.replace(surface.clone());
        if !again {
            *self.owed.displaced.borrow_mut() = displaced;
        }
        self.owed.shown.set(Some(surface.id()));
        self.owed.refreshes.set(0);
        if let Some(l) = self.owed.link.borrow().as_ref() {
            l.setPaused(false);
        }
        #[cfg(feature = "latency-trace")]
        {
            self.check.1 = Some(std::time::Instant::now());
            self.check.2 += 1;
            if trace::enabled() {
                trace::mark(Stage::PresentEnd, 0);
                trace::mark_at(Stage::SurfaceHash, surface.pixel_hash(), 0);
                trace::flush();
            }
        }
    }

    /// Once a frame has been up for a moment: say in the log how the
    /// window as composited compares with it.
    #[cfg(feature = "latency-trace")]
    pub(crate) fn check_screen(&mut self) {
        if !self.check.1.is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(300)) {
            return;
        }
        self.check.1 = None;
        let surface =
            self.on_window.clone().or_else(|| self.legacy.as_ref().and_then(|p| IOSurface::lookup(p.current_id())));
        let (Some(surface), Some(window)) = (surface, self.check.0.window()) else { return };
        match crate::screen_check::check(window.windowNumber() as u32, &surface) {
            Some(seen) => marspot::lx_event!(
                "SHOWN_CHECK",
                "the window as composited against the surface last shown",
                window_id = self.owed.window_id,
                surface_id = surface.id(),
                copied = self.legacy.is_some(),
                frames = self.check.2,
                upright = format!("{:.4}", seen.upright),
                flipped = format!("{:.4}", seen.flipped),
                surface_px = format!("{}x{}", surface.width(), surface.height()),
                capture_px = format!("{}x{}", seen.capture.0, seen.capture.1)
            ),
            None => marspot::lx_event!("SHOWN_CHECK_NO_CAPTURE", "the window could not be captured", frames = self.check.2),
        }
    }

    /// The window's size or its display's scale changed.
    pub(crate) fn resized(&mut self, w_phys: f64, h_phys: f64, scale: f64) {
        if scale != self.scale {
            self.scale = scale;
            if let Some(s) = self.on_window.as_ref() {
                let size = NSSize::new(s.width() as f64 / scale, s.height() as f64 / scale);
                unanimated(|| self.frame.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size)));
            }
        }
        if let Some(p) = self.legacy.as_ref() {
            p.set_drawable_size(w_phys, h_phys, scale);
        }
    }

    /// The presenter that copies frames, for a core that cannot be
    /// paced: made on first use, sampling `front` and `back`.
    pub(crate) fn legacy(&mut self, front: &IOSurface, back: &IOSurface) -> Result<&mut ShellPresenter, String> {
        if self.legacy.is_none() {
            let p = ShellPresenter::new(self.scale as f32, front, back)?;
            let bounds = self.root.bounds();
            p.set_drawable_size(bounds.size.width * self.scale, bounds.size.height * self.scale, self.scale);
            unanimated(|| {
                p.layer.setFrame(bounds);
                p.layer.setAutoresizingMask(CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable);
                // above the frame layer, below a banner
                self.root.insertSublayer_above(&p.layer, Some(&self.frame));
            });
            self.legacy = Some(p);
        }
        Ok(self.legacy.as_mut().expect("made above"))
    }

    /// The copying presenter, when this window has one.
    pub(crate) fn copying(&mut self) -> Option<&mut ShellPresenter> {
        self.legacy.as_mut()
    }

    /// The copy went up: the frame layer under it lets go of the last
    /// surface a pacing core left there.
    pub(crate) fn copied(&mut self) {
        #[cfg(feature = "latency-trace")]
        {
            self.check.1 = Some(std::time::Instant::now());
            self.check.2 += 1;
        }
        if self.on_window.take().is_some() {
            // SAFETY: no contents.
            unanimated(|| unsafe { self.frame.setContents(None) });
        }
    }

    /// Replace the banner over the window, or take it down with `None`.
    pub(crate) fn set_banner(&mut self, kind: Option<BannerKind>, scale: f64) -> Result<(), String> {
        let image = kind.map(|k| crate::banner::rasterise(k.text(), 14.0, scale)).transpose()?;
        unanimated(|| {
            if let Some(old) = self.banner.take() {
                old.removeFromSuperlayer();
            }
            let Some(image) = image else { return };
            let banner = CALayer::new();
            banner.setBounds(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(image.width_px as f64 / scale, image.height_px as f64 / scale),
            ));
            let within = self.root.bounds();
            banner.setPosition(NSPoint::new(
                within.origin.x + within.size.width / 2.0,
                within.origin.y + within.size.height / 2.0,
            ));
            // stays centred as the window is resized
            banner.setAutoresizingMask(
                CAAutoresizingMask::LayerMinXMargin
                    | CAAutoresizingMask::LayerMaxXMargin
                    | CAAutoresizingMask::LayerMinYMargin
                    | CAAutoresizingMask::LayerMaxYMargin,
            );
            let cg: *const AnyObject = foreign_types::ForeignType::as_ptr(&image.image).cast();
            // SAFETY: a CGImage is an object a layer takes as contents,
            // and the layer retains it.
            unsafe { banner.setContents(Some(&*cg)) };
            self.root.addSublayer(&banner);
            self.banner = Some(banner);
        });
        Ok(())
    }
}

impl Drop for FrameLayers {
    fn drop(&mut self) {
        // the link holds its target, which holds the link
        if let Some(l) = self.owed.link.borrow_mut().take() {
            l.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owed() -> Owed {
        Owed {
            window_id: 3,
            shown: Cell::new(None),
            displaced: RefCell::new(None),
            refreshes: Cell::new(0),
            link: RefCell::new(None),
        }
    }

    /// What the core is sent, read back off the other end of a socket.
    fn reports(of: impl FnOnce()) -> Vec<(u32, u32)> {
        let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
        set_core(Some(ours));
        of();
        set_core(None);
        let mut got = Vec::new();
        while let Ok(Some(f)) = Frame::read_from(&mut theirs) {
            assert_eq!(f.msg_type, MsgType::FrameShown);
            got.push(marspot::shell_proto::decode_frame_shown(&f.payload).expect("a report"));
        }
        got
    }

    #[test]
    fn a_swap_is_reported_once_at_the_refresh_after_it() {
        let o = owed();
        let got = reports(|| {
            o.refresh();
            o.shown.set(Some(41));
            o.refresh();
            o.refresh();
        });
        assert_eq!(got, vec![(3, 41)]);
    }

    /// The window server holds a use count on a surface it composites
    /// from; the test holds one the same way.
    #[test]
    fn a_surface_still_on_the_screen_holds_the_report_one_refresh() {
        let old = IOSurface::create(8, 8).expect("a surface");
        old.increment_use();
        let o = owed();
        let first = reports(|| {
            o.shown.set(Some(42));
            *o.displaced.borrow_mut() = Some(old.clone());
            o.refresh();
        });
        assert!(first.is_empty(), "reported while the old surface was in use: {first:?}");
        let second = reports(|| o.refresh());
        assert_eq!(second, vec![(3, 42)], "a second refresh reports whatever the use count says");
        old.decrement_use();
    }

    #[test]
    fn a_surface_the_window_server_let_go_of_is_reported_at_the_first_refresh() {
        let old = IOSurface::create(8, 8).expect("a surface");
        let o = owed();
        let got = reports(|| {
            o.shown.set(Some(43));
            *o.displaced.borrow_mut() = Some(old.clone());
            o.refresh();
        });
        assert_eq!(got, vec![(3, 43)]);
    }
}
