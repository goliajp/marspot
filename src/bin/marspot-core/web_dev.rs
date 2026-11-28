//! Dev seam: the web pane script (`marspot::web_pane::dev_script`),
//! played into the first window as the events the window would send,
//! routed and handled like any others.

use std::sync::Mutex;

use marspot::input::{KeyState, LogicalKey};
use marspot::pane::PaneBackend;
use marspot::ui::components::panel_script::ChordKey;
use marspot::web_pane::dev_script::{Script, Step};

use super::page_rect;
use super::*;

/// The script, once the dev web pane is open; none in the installed app.
static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);

fn script() -> std::sync::MutexGuard<'static, Option<Script>> {
    SCRIPT.lock().unwrap_or_else(|p| p.into_inner())
}

impl CoreApp {
    pub(crate) fn dev_web_script_start(&mut self) {
        *script() = Script::from_env();
    }

    /// Whether the loop must come round soon though nothing woke it.
    pub(crate) fn dev_ticking(&self) -> bool {
        self.dev_panels.is_some() || script().is_some()
    }

    pub(crate) fn dev_web_script_play(&mut self) {
        let step = {
            let mut s = script();
            let Some(sc) = s.as_mut() else { return };
            let step = sc.next(Instant::now());
            if sc.finished() {
                *s = None;
                lx_event!("DEV_WEB_SCRIPT_DONE", "the web pane script has been played");
            }
            step
        };
        if let Some(step) = step {
            lx_event!("DEV_WEB_STEP", "playing a web pane step", step = format!("{step:?}"));
            self.dev_web_step(step);
        }
    }

    fn dev_web_step(&mut self, step: Step) {
        let Some(w) = self.windows.first() else { return };
        let id = w.window_id;
        let i = w.panes.iter().position(|p| matches!(p.session(), PaneBackend::Web(_)));
        let Some((x0, y0, _, _)) = i.and_then(|i| page_rect(&w.layout, i)) else { return };
        let d = marspot::layout::density();
        let at = |x: f64, y: f64| (x0 + x * d, y0 + y * d);
        let cmd = Modifiers { super_: true, ..Default::default() };
        let events = match step {
            Step::Until(_) | Step::Wait(_) => vec![],
            Step::Scroll(dx, dy) => {
                // what the trackpad sends to move the page that far (`scroll_xy`)
                let sign = if marspot::ui::scroll_config().0 { 1.0 } else { -1.0 };
                vec![CoreEvent::Scroll(sign * dx * d, sign * dy * d, true, id)]
            }
            Step::Key(k, mods) => {
                let (logical, text) = match k {
                    ChordKey::Named(n) => (LogicalKey::Named(n), None),
                    ChordKey::Char(c) => (LogicalKey::Char(c), (!mods.super_ && !mods.control).then(|| c.to_string())),
                };
                [KeyState::Pressed, KeyState::Released]
                    .into_iter()
                    .map(|state| {
                        let e = MarspotKeyEvent { state, logical: logical.clone(), text: text.as_deref().map(Into::into), ..Default::default() };
                        CoreEvent::Key(e, mods, id)
                    })
                    .collect()
            }
            Step::Move(x, y) => {
                let (x, y) = at(x, y);
                vec![CoreEvent::MouseMove(x, y, id)]
            }
            Step::Click(x, y, with_cmd) => {
                let (x, y) = at(x, y);
                let m = if with_cmd { cmd } else { Modifiers::default() };
                vec![CoreEvent::MouseMove(x, y, id), CoreEvent::MouseDown(x, y, m, id), CoreEvent::MouseUp(id, id, x, y)]
            }
            Step::Drag(xa, ya, xb, yb) => {
                let ((xa, ya), (xb, yb)) = (at(xa, ya), at(xb, yb));
                vec![
                    CoreEvent::MouseDown(xa, ya, Modifiers::default(), id),
                    CoreEvent::MouseDrag(xb, yb, id, id, xb, yb),
                    CoreEvent::MouseUp(id, id, xb, yb),
                ]
            }
            Step::Minimize(on) => vec![CoreEvent::WindowMinimized(id, on)],
        };
        for e in events {
            let _ = self.event_tx.send(e);
        }
    }
}
