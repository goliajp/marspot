//! A script of pointer and key input for the panels, played into a
//! sandboxed app so a real window's handling of them can be checked.
//!
//! Nothing outside the app can press a toolbar button or drag a panel
//! by its header, and what the rules in `surface_stack` decide only
//! matters once real presses and keys are routed to them.  A script
//! is turned here into the same events the window would have sent, at
//! points read off the layout as it is when each step runs; the
//! front-end's only part is to queue them where input arrives.
//!
//! `MARSPOT_DEV_PANELS` holds the script and is read only when
//! `MARSPOT_DEV_SANDBOX` is set, so the installed app never plays one.
//! Steps are separated by `;`:
//!
//! | step | what it does |
//! |---|---|
//! | `button:<settings\|layout\|process\|accounts>` | clicks that panel's toolbar button |
//! | `key:<chord>` | the chord, pressed and released: any of `cmd-`, `shift-`, `ctrl-`, `alt-`, then one character or `esc`, `enter`, `tab`, `up`, `down`, `left`, `right`, `f1`–`f12` (`key:cmd-,`, `key:cmd-shift-l`, `key:ctrl-c`, `key:f5`) |
//! | `esc` | short for `key:esc` |
//! | `type:<char>` | that character, pressed and released |
//! | `click:close` | clicks the open panel's close button |
//! | `click:header` | clicks the open panel's header |
//! | `click:outside` | clicks in a pane, outside the open panel |
//! | `click:pane-under` | clicks inside the open panel's frame, over a pane |
//! | `click:helpers-mark` | clicks the `+N` after the first pane's title |
//! | `click:helpers-view`, `-move`, `-close`, `-close-all`, `-control` | clicks the first such button of the open helper panel |
//! | `click:pane` | clicks in the first pane |
//! | `shift-click:<target>` | any of those clicks with shift held, the click a program that asked for mouse reports is sent |
//! | `move:<target>` | moves the pointer there, pressing nothing |
//! | `pointer-exit` | moves the pointer out of the window |
//! | `drag:header:<dx>,<dy>` | drags the open panel by its header |
//! | `scroll:<lines>` | turns the wheel |
//! | `wait:<ms>` | does nothing for that long |
//! | `until:<name>` | waits until the state directory holds a file of that name |

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::layout::{Layout, Rect};

use crate::ui::components::panel_chrome::PanelChrome;
use crate::ui::components::surface_stack::SurfaceKind;

/// How long after boot, and after finding no panes yet, the first
/// step waits: restored panes land seconds after the window does.
const START_DELAY: Duration = Duration::from_secs(3);
/// Between steps — long enough for the events of one to be handled,
/// and drawn, before the next reads the layout.
const STEP_GAP: Duration = Duration::from_millis(300);
/// How often an `until` step looks again.
const UNTIL_GAP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Close,
    Header,
    Outside,
    PaneUnder,
    Pane,
    /// The first pane's `+N`, then the first button of each kind on the
    /// helper panel: `Scene::helpers`, by place.
    Helpers(u8),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Button(SurfaceKind),
    Key(ChordKey, Modifiers),
    Type(char),
    Click(Target),
    ShiftClick(Target),
    Move(Target),
    PointerExit,
    DragHeader(f64, f64),
    Scroll(f64),
    Wait(u64),
    Until(String),
}

/// The key of a `key:` step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChordKey {
    Char(char),
    Named(NamedKey),
}

pub const ESC: Step = Step::Key(ChordKey::Named(NamedKey::Escape), NO_MODIFIERS);
const NO_MODIFIERS: Modifiers = Modifiers { shift: false, control: false, alt: false, super_: false };

/// One event for the front-end to queue as if the window had sent it.
#[derive(Debug, Clone)]
pub enum Input {
    Move(f64, f64),
    Down(f64, f64, Modifiers),
    Drag(f64, f64),
    Up(f64, f64),
    /// Physical pixels, as a trackpad reports them.
    Scroll(f64),
    Key(MarspotKeyEvent, Modifiers),
}

/// The window a step is aimed at, as it is right now.
pub struct Scene<'a> {
    pub layout: &'a Layout,
    /// The open panel's chrome.
    pub chrome: Option<PanelChrome>,
    /// The renderer's cell size.
    pub cell: (f64, f64),
    /// Where `Target::Helpers` are.
    pub helpers: [Option<(f64, f64)>; 6],
}

pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    script.split(';').map(str::trim).filter(|s| !s.is_empty()).map(parse_step).collect()
}

fn parse_step(text: &str) -> Result<Step, String> {
    let bad = || format!("unknown step `{text}`");
    let (verb, arg) = text.split_once(':').unwrap_or((text, ""));
    Ok(match (verb, arg) {
        ("esc", "") => ESC,
        ("key", chord) => parse_chord(chord).ok_or_else(bad)?,
        ("button", "settings") => Step::Button(SurfaceKind::Settings),
        ("button", "layout") => Step::Button(SurfaceKind::Layout),
        ("button", "process") => Step::Button(SurfaceKind::Processes),
        ("button", "accounts") => Step::Button(SurfaceKind::Accounts),
        ("click", t) => Step::Click(parse_target(t).ok_or_else(bad)?),
        ("shift-click", t) => Step::ShiftClick(parse_target(t).ok_or_else(bad)?),
        ("move", t) => Step::Move(parse_target(t).ok_or_else(bad)?),
        ("pointer-exit", "") => Step::PointerExit,
        ("type", c) => {
            let mut chars = c.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Step::Type(c),
                _ => return Err(bad()),
            }
        }
        ("drag", d) => {
            let (dx, dy) =
                d.strip_prefix("header:").and_then(|d| d.split_once(',')).ok_or_else(bad)?;
            Step::DragHeader(dx.parse().map_err(|_| bad())?, dy.parse().map_err(|_| bad())?)
        }
        ("scroll", n) => Step::Scroll(n.parse().map_err(|_| bad())?),
        ("until", name) if !name.is_empty() && !name.contains('/') => Step::Until(name.to_string()),
        ("wait", ms) => Step::Wait(ms.parse().map_err(|_| bad())?),
        _ => return Err(bad()),
    })
}

fn parse_chord(text: &str) -> Option<Step> {
    let mut mods = NO_MODIFIERS;
    let mut rest = text;
    // A lone `-` after the modifiers is the key itself.
    while let Some((name, after)) = rest.split_once('-').filter(|(_, after)| !after.is_empty()) {
        match name {
            "cmd" => mods.super_ = true,
            "shift" => mods.shift = true,
            "ctrl" => mods.control = true,
            "alt" => mods.alt = true,
            _ => return None,
        }
        rest = after;
    }
    let named = match rest {
        "esc" => Some(NamedKey::Escape),
        "enter" => Some(NamedKey::Enter),
        "tab" => Some(NamedKey::Tab),
        "up" => Some(NamedKey::ArrowUp),
        "down" => Some(NamedKey::ArrowDown),
        "left" => Some(NamedKey::ArrowLeft),
        "right" => Some(NamedKey::ArrowRight),
        _ => rest.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()).and_then(|n| {
            use NamedKey::*;
            [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12].get(n.checked_sub(1)?).copied()
        }),
    };
    let mut chars = rest.chars();
    let key = match (named, chars.next(), chars.next()) {
        (Some(n), _, _) => ChordKey::Named(n),
        (None, Some(c), None) => ChordKey::Char(c.to_ascii_lowercase()),
        _ => return None,
    };
    Some(Step::Key(key, mods))
}

fn parse_target(text: &str) -> Option<Target> {
    Some(match text {
        "close" => Target::Close,
        "header" => Target::Header,
        "outside" => Target::Outside,
        "pane-under" => Target::PaneUnder,
        "pane" => Target::Pane,
        "helpers-mark" => Target::Helpers(0),
        "helpers-move" => Target::Helpers(1),
        "helpers-close" => Target::Helpers(2),
        "helpers-close-all" => Target::Helpers(3),
        "helpers-control" => Target::Helpers(4),
        "helpers-view" => Target::Helpers(5),
        _ => return None,
    })
}

fn centre(r: Rect) -> (f64, f64) {
    (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
}

fn click((x, y): (f64, f64), modifiers: Modifiers) -> Vec<Input> {
    vec![Input::Move(x, y), Input::Down(x, y, modifiers), Input::Up(x, y)]
}

fn key(logical: LogicalKey, text: Option<String>, modifiers: Modifiers) -> Vec<Input> {
    [KeyState::Pressed, KeyState::Released]
        .into_iter()
        .map(|state| {
            let event =
                MarspotKeyEvent { state, logical: logical.clone(), text: text.clone(), ..Default::default() };
            Input::Key(event, modifiers)
        })
        .collect()
}

impl Scene<'_> {
    fn chrome(&self) -> Result<PanelChrome, String> {
        self.chrome.ok_or_else(|| "no panel is open".to_string())
    }

    /// Is `(x, y)` on a pane's terminal area, where a press would be
    /// the program's?
    fn on_a_pane(&self, x: f64, y: f64) -> bool {
        self.layout.hit_test_cell_pos(x, y, self.cell.0, self.cell.1).is_some()
    }

    /// A point on some pane's terminal area that `ok` accepts.
    fn pane_point(&self, ok: impl Fn(f64, f64) -> bool) -> Option<(f64, f64)> {
        let l = self.layout;
        let spots = [(0.5, 0.5), (0.1, 0.1), (0.9, 0.1), (0.1, 0.9), (0.9, 0.9)];
        l.cells.iter().find_map(|c| {
            let (x0, y0) = (c.x + l.padding, c.y_top + l.cell_title_h + l.padding);
            let (w, h) = (c.x + c.w - x0, c.y_top + c.h - y0);
            spots
                .iter()
                .map(|(fx, fy)| ((x0 + w * fx).floor(), (y0 + h * fy).floor()))
                .find(|&(x, y)| self.on_a_pane(x, y) && ok(x, y))
        })
    }

    fn point(&self, target: Target) -> Result<(f64, f64), String> {
        match target {
            Target::Close => Ok(centre(self.chrome()?.close)),
            Target::Header => {
                let c = self.chrome()?;
                // Left of the close button, which is not a handle.
                Ok(((c.header.x + c.close.x) * 0.5, c.header.y_top + c.header.h * 0.5))
            }
            Target::Outside => {
                let frame = self.chrome()?.frame;
                self.pane_point(|x, y| !frame.contains(x, y))
                    .ok_or_else(|| "the panel covers every pane".to_string())
            }
            // The strip under the panel's content: inside the frame,
            // and on nothing of the panel's own that a press would set
            // off.
            Target::PaneUnder => {
                let f = self.chrome()?.frame;
                let y = f.y_top + f.h - 2.0;
                [0.5, 0.25, 0.75, 0.1, 0.9]
                    .iter()
                    .map(|fx| (f.x + f.w * fx).floor())
                    .find(|&x| f.contains(x, y) && self.on_a_pane(x, y))
                    .map(|x| (x, y))
                    .ok_or_else(|| "no pane under the panel's lower edge".to_string())
            }
            Target::Helpers(i) => self
                .helpers
                .get(i as usize)
                .copied()
                .flatten()
                .ok_or_else(|| "no such spot of the helpers".to_string()),
            Target::Pane => self.pane_point(|_, _| true).ok_or_else(|| "no pane".to_string()),
        }
    }

    /// The events `step` is made of, in the order the window would
    /// send them.
    pub fn resolve(&self, step: &Step) -> Result<Vec<Input>, String> {
        let l = self.layout;
        Ok(match *step {
            Step::Button(kind) => {
                let rect = match kind {
                    SurfaceKind::Settings => l.settings_button_rect,
                    SurfaceKind::Layout => l.layout_button_rect,
                    SurfaceKind::Processes => l.process_button_rect,
                    SurfaceKind::Accounts => l.cc_button_rect,
                    SurfaceKind::Shortcuts | SurfaceKind::Helpers => Rect { x: 0.0, y_top: 0.0, w: 0.0, h: 0.0 },
                };
                if rect.w <= 0.0 || rect.h <= 0.0 {
                    return Err("the toolbar has no such button".to_string());
                }
                click(centre(rect), Modifiers::default())
            }
            Step::Key(ChordKey::Named(n), mods) => key(LogicalKey::Named(n), None, mods),
            Step::Key(ChordKey::Char(c), mods) => {
                // What the window reports the key as typing: nothing
                // the encoder reads under Cmd or Ctrl, the capital
                // under shift.
                let typed = if mods.shift { c.to_ascii_uppercase() } else { c };
                let text = (!mods.super_ && !mods.control).then(|| typed.to_string());
                key(LogicalKey::Char(c), text, mods)
            }
            Step::Type(c) => key(LogicalKey::Char(c), Some(c.to_string()), Modifiers::default()),
            Step::Click(target) => click(self.point(target)?, Modifiers::default()),
            Step::ShiftClick(target) => {
                click(self.point(target)?, Modifiers { shift: true, ..Default::default() })
            }
            Step::Move(target) => {
                let (x, y) = self.point(target)?;
                vec![Input::Move(x, y)]
            }
            Step::PointerExit => {
                let (x, y) = crate::app_pointer::OUTSIDE;
                vec![Input::Move(x, y)]
            }
            Step::DragHeader(dx, dy) => {
                let (x, y) = self.point(Target::Header)?;
                vec![
                    Input::Move(x, y),
                    Input::Down(x, y, Modifiers::default()),
                    Input::Drag(x + dx * 0.5, y + dy * 0.5),
                    Input::Drag(x + dx, y + dy),
                    Input::Up(x + dx, y + dy),
                ]
            }
            Step::Scroll(lines) => vec![Input::Scroll(lines * self.cell.1)],
            Step::Wait(_) | Step::Until(_) => Vec::new(),
        })
    }
}

/// A script being played, one step at a time.
pub struct Script {
    steps: VecDeque<Step>,
    next_at: Instant,
    finished: bool,
}

impl Script {
    pub fn new(steps: Vec<Step>, now: Instant) -> Self {
        Self { steps: steps.into(), next_at: now + START_DELAY, finished: false }
    }

    /// The script the environment asks for, in a sandbox only.
    pub fn from_env() -> Option<Self> {
        std::env::var_os("MARSPOT_DEV_SANDBOX")?;
        let text = std::env::var("MARSPOT_DEV_PANELS").ok()?;
        match parse(&text) {
            Ok(steps) => Some(Self::new(steps, Instant::now())),
            Err(why) => {
                crate::lx_event!("DEV_PANELS_REJECTED", "the panel script does not parse", why = why);
                None
            }
        }
    }

    /// Is it time for the next step?
    pub fn due(&self, now: Instant) -> bool {
        !self.finished && now >= self.next_at
    }

    /// The next step is aimed at a [`Target::Helpers`].
    pub fn wants_helpers(&self) -> bool {
        matches!(self.steps.front(), Some(Step::Click(Target::Helpers(_)) | Step::Move(Target::Helpers(_))))
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Takes the next step and returns its events.  Call when
    /// [`due`](Self::due); the step after it is due a moment later,
    /// once these have been handled.
    pub fn play(&mut self, now: Instant, scene: &Scene) -> Vec<Input> {
        if scene.layout.cells.is_empty() {
            self.next_at = now + START_DELAY;
            return Vec::new();
        }
        let Some(step) = self.steps.pop_front() else {
            self.finished = true;
            crate::lx_event!("DEV_PANELS_DONE", "the panel script has been played");
            return Vec::new();
        };
        if let Step::Until(name) = &step
            && !marspot_term::paths::state_root().join(name).exists()
        {
            self.steps.push_front(step);
            self.next_at = now + UNTIL_GAP;
            return Vec::new();
        }
        self.next_at = now + match step {
            Step::Wait(ms) => Duration::from_millis(ms),
            _ => STEP_GAP,
        };
        match scene.resolve(&step) {
            Ok(inputs) => {
                crate::lx_event!("DEV_PANELS_STEP", "playing a step", step = format!("{step:?}"));
                inputs
            }
            Err(why) => {
                crate::lx_event!(
                    "DEV_PANELS_STEP_FAILED",
                    "a step has nowhere to go",
                    step = format!("{step:?}"),
                    why = why
                );
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
#[path = "panel_script_tests.rs"]
mod tests;
