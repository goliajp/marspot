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
//! | `click:pane` | clicks in the first pane |
//! | `shift-click:<target>` | any of those clicks with shift held, the click a program that asked for mouse reports is sent |
//! | `drag:header:<dx>,<dy>` | drags the open panel by its header |
//! | `scroll:<lines>` | turns the wheel |
//! | `wait:<ms>` | does nothing for that long |

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Close,
    Header,
    Outside,
    PaneUnder,
    Pane,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Button(SurfaceKind),
    Key(ChordKey, Modifiers),
    Type(char),
    Click(Target),
    ShiftClick(Target),
    DragHeader(f64, f64),
    Scroll(f64),
    Wait(u64),
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
            Target::Pane => self.pane_point(|_, _| true).ok_or_else(|| "no pane".to_string()),
        }
    }

    /// The events `step` is made of, in the order the window would
    /// send them.
    pub fn resolve(&self, step: Step) -> Result<Vec<Input>, String> {
        let l = self.layout;
        Ok(match step {
            Step::Button(kind) => {
                let rect = match kind {
                    SurfaceKind::Settings => l.settings_button_rect,
                    SurfaceKind::Layout => l.layout_button_rect,
                    SurfaceKind::Processes => l.process_button_rect,
                    SurfaceKind::Accounts => l.cc_button_rect,
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
            Step::Wait(_) => Vec::new(),
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
        self.next_at = now + match step {
            Step::Wait(ms) => Duration::from_millis(ms),
            _ => STEP_GAP,
        };
        match scene.resolve(step) {
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
mod tests {
    use super::*;

    const CELL: (f64, f64) = (8.0, 16.0);

    fn layout() -> Layout {
        Layout::build(1600.0, 1000.0, 200.0, 56.0, 24.0, 2, 1, CELL.0, CELL.1)
            .with_chrome_showing(2.0, 2, None, false)
    }

    fn panel() -> PanelChrome {
        PanelChrome::new(Rect { x: 400.0, y_top: 200.0, w: 800.0, h: 600.0 }, 56.0)
    }

    #[test]
    fn a_script_parses_into_its_steps() {
        let steps = parse(
            "button:settings; esc;key:cmd-w;click:close;click:outside;click:pane-under;\
             click:pane;click:header;shift-click:pane-under;drag:header:120,-40;scroll:3;type:x;wait:500;",
        )
        .unwrap();
        assert_eq!(
            steps,
            [
                Step::Button(SurfaceKind::Settings),
                ESC,
                Step::Key(ChordKey::Char('w'), cmd()),
                Step::Click(Target::Close),
                Step::Click(Target::Outside),
                Step::Click(Target::PaneUnder),
                Step::Click(Target::Pane),
                Step::Click(Target::Header),
                Step::ShiftClick(Target::PaneUnder),
                Step::DragHeader(120.0, -40.0),
                Step::Scroll(3.0),
                Step::Type('x'),
                Step::Wait(500),
            ]
        );
        for kind in ["settings", "layout", "process", "accounts"] {
            assert!(parse(&format!("button:{kind}")).is_ok(), "{kind}");
        }
        assert_eq!(parse(""), Ok(Vec::new()));
    }

    fn cmd() -> Modifiers {
        Modifiers { super_: true, ..Default::default() }
    }

    #[test]
    fn a_chord_is_its_modifiers_then_its_key() {
        let chord = |text: &str| match parse(&format!("key:{text}")).unwrap()[..] {
            [Step::Key(key, m)] => (key, (m.super_, m.shift, m.control, m.alt)),
            ref other => panic!("{text}: {other:?}"),
        };
        let c = ChordKey::Char;
        assert_eq!(chord("cmd-,"), (c(','), (true, false, false, false)));
        assert_eq!(chord("cmd-shift-l"), (c('l'), (true, true, false, false)));
        assert_eq!(chord("cmd-shift-L"), chord("cmd-shift-l"));
        assert_eq!(chord("cmd-2"), (c('2'), (true, false, false, false)));
        assert_eq!(chord("cmd-]"), (c(']'), (true, false, false, false)));
        assert_eq!(chord("cmd--"), (c('-'), (true, false, false, false)));
        assert_eq!(chord("-"), (c('-'), (false, false, false, false)));
        assert_eq!(chord("ctrl-c"), (c('c'), (false, false, true, false)));
        assert_eq!(chord("alt-b"), (c('b'), (false, false, false, true)));
        assert_eq!(chord("ctrl-cmd-f"), (c('f'), (true, false, true, false)));
        assert_eq!(chord("f"), (c('f'), (false, false, false, false)));
        assert_eq!(chord("f5").0, ChordKey::Named(NamedKey::F5));
        assert_eq!(chord("f12").0, ChordKey::Named(NamedKey::F12));
        assert_eq!(chord("up").0, ChordKey::Named(NamedKey::ArrowUp));
        assert_eq!(parse("key:esc").unwrap(), [ESC]);
    }

    #[test]
    fn a_step_it_does_not_know_rejects_the_whole_script() {
        for bad in [
            "button:nope", "click", "click:middle", "shift-click:", "drag:120,40", "drag:header:1", "type:",
            "type:xy", "scroll:x", "key:", "key:cmd-", "key:hyper-x", "key:cmd-xy", "key:f13", "key:f0", "esc:1",
            "wait:soon",
        ] {
            let script = format!("esc;{bad};esc");
            assert_eq!(parse(&script), Err(format!("unknown step `{bad}`")), "{bad}");
        }
    }

    #[test]
    fn a_button_step_presses_the_button_the_toolbar_hit_tests() {
        let l = layout();
        let scene = Scene { layout: &l, chrome: None, cell: CELL };
        let hits: [(SurfaceKind, fn(&Layout, f64, f64) -> bool); 4] = [
            (SurfaceKind::Settings, Layout::hit_test_settings_button),
            (SurfaceKind::Layout, Layout::hit_test_layout_button),
            (SurfaceKind::Processes, Layout::hit_test_process_button),
            (SurfaceKind::Accounts, Layout::hit_test_cc_button),
        ];
        for (kind, hit) in hits {
            let inputs = scene.resolve(Step::Button(kind)).unwrap();
            let [Input::Move(mx, my), Input::Down(x, y, mods), Input::Up(ux, uy)] = inputs[..] else {
                panic!("{kind:?}: {inputs:?}");
            };
            assert!(hit(&l, x, y) && !mods.shift, "{kind:?}");
            assert_eq!(((mx, my), (ux, uy)), ((x, y), (x, y)), "{kind:?}");
        }
    }

    #[test]
    fn targets_land_where_their_names_say() {
        let l = layout();
        let c = panel();
        let scene = Scene { layout: &l, chrome: Some(c), cell: CELL };
        let at = |target| scene.point(target).unwrap();

        let (x, y) = at(Target::Close);
        assert!(c.hit_close(x, y));
        let (x, y) = at(Target::Header);
        assert!(c.hit_header(x, y));
        let (x, y) = at(Target::Outside);
        assert!(!c.frame.contains(x, y) && scene.on_a_pane(x, y));
        let (x, y) = at(Target::PaneUnder);
        assert!(c.frame.contains(x, y) && scene.on_a_pane(x, y));
        assert!(!c.content.contains(x, y) && !c.header.contains(x, y), "on nothing of the panel's");
        let (x, y) = at(Target::Pane);
        assert_eq!(l.hit_test_cell_pos(x, y, CELL.0, CELL.1).map(|(i, _, _)| i), Some(0));
    }

    #[test]
    fn a_panel_target_needs_a_panel() {
        let l = layout();
        let scene = Scene { layout: &l, chrome: None, cell: CELL };
        for target in [Target::Close, Target::Header, Target::Outside, Target::PaneUnder] {
            assert!(scene.resolve(Step::Click(target)).is_err(), "{target:?}");
        }
        assert!(scene.resolve(Step::DragHeader(1.0, 1.0)).is_err());
        assert!(scene.resolve(Step::Click(Target::Pane)).is_ok());
        let held = |step| match scene.resolve(step).unwrap()[1] {
            Input::Down(_, _, mods) => mods.shift,
            ref other => panic!("{other:?}"),
        };
        assert!(held(Step::ShiftClick(Target::Pane)) && !held(Step::Click(Target::Pane)));
    }

    #[test]
    fn a_drag_ends_the_asked_distance_from_where_it_began() {
        let l = layout();
        let scene = Scene { layout: &l, chrome: Some(panel()), cell: CELL };
        let inputs = scene.resolve(Step::DragHeader(120.0, 40.0)).unwrap();
        let (Some(Input::Down(x, y, _)), Some(Input::Up(ux, uy))) = (inputs.get(1), inputs.last())
        else {
            panic!("{inputs:?}");
        };
        assert_eq!((ux - x, uy - y), (120.0, 40.0));
        assert!(matches!(inputs[inputs.len() - 2], Input::Drag(x, y) if (x, y) == (*ux, *uy)));
    }

    #[test]
    fn keys_come_pressed_then_released() {
        let l = layout();
        let scene = Scene { layout: &l, chrome: None, cell: CELL };
        let keys = |step| -> Vec<(KeyState, LogicalKey, Option<String>, bool)> {
            scene
                .resolve(step)
                .unwrap()
                .into_iter()
                .map(|i| match i {
                    Input::Key(e, m) => (e.state, e.logical, e.text, m.super_),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        let pair = |logical: LogicalKey, text: Option<&str>, cmd| {
            [KeyState::Pressed, KeyState::Released]
                .map(|s| (s, logical.clone(), text.map(str::to_string), cmd))
                .to_vec()
        };
        let step = |text: &str| parse(text).unwrap()[0];
        assert_eq!(keys(ESC), pair(LogicalKey::Named(NamedKey::Escape), None, false));
        assert_eq!(keys(step("key:cmd-w")), pair(LogicalKey::Char('w'), None, true));
        assert_eq!(keys(step("key:ctrl-c")), pair(LogicalKey::Char('c'), None, false));
        assert_eq!(keys(step("key:alt-b")), pair(LogicalKey::Char('b'), Some("b"), false));
        assert_eq!(keys(step("key:shift-a")), pair(LogicalKey::Char('a'), Some("A"), false));
        assert_eq!(keys(step("key:f5")), pair(LogicalKey::Named(NamedKey::F5), None, false));
        assert_eq!(keys(Step::Type('x')), pair(LogicalKey::Char('x'), Some("x"), false));
        assert!(matches!(scene.resolve(Step::Scroll(3.0)).unwrap()[..], [Input::Scroll(dy)] if dy == 48.0));
    }

    #[test]
    fn steps_are_played_one_at_a_time_and_then_it_is_finished() {
        let l = layout();
        let scene = Scene { layout: &l, chrome: None, cell: CELL };
        let t0 = Instant::now();
        let mut s = Script::new(parse("esc;wait:2000;click:close;type:x").unwrap(), t0);
        assert!(!s.due(t0), "not before the window has had time to fill");
        let t = t0 + START_DELAY;
        assert!(s.due(t));

        // No panes yet: nothing is consumed, and it looks again later.
        let mut empty = layout();
        empty.cells.clear();
        assert!(s.play(t, &Scene { layout: &empty, chrome: None, cell: CELL }).is_empty());
        assert!(!s.due(t + STEP_GAP));
        let t = t + START_DELAY;

        assert_eq!(s.play(t, &scene).len(), 2);
        assert!(!s.due(t) && s.due(t + STEP_GAP));
        assert!(s.play(t, &scene).is_empty(), "the wait");
        assert!(!s.due(t + STEP_GAP) && s.due(t + Duration::from_millis(2000)));
        // A step with nowhere to go is skipped, not fatal.
        assert!(s.play(t, &scene).is_empty());
        assert_eq!(s.play(t, &scene).len(), 2);
        assert!(!s.finished());
        assert!(s.play(t, &scene).is_empty());
        assert!(s.finished() && !s.due(t + START_DELAY));
    }
}
