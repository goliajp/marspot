//! Dev seam: a script of input for the first web pane, played into a
//! sandboxed app so the window suite can drive a page as a person would
//! (`MARSPOT_DEV_WEB_SCRIPT`, honoured where the other dev seams are).
//! The core turns each step into the events the window would have sent
//! and routes them as it routes those.  Steps are separated by `;`;
//! points and distances are CSS px of the page:
//!
//! | step | what it does |
//! |---|---|
//! | `until:<name>` | waits until the state directory holds a file of that name |
//! | `wait:<ms>` | does nothing for that long |
//! | `scroll:<dx>,<dy>` | the trackpad, moving the page that far |
//! | `wheel:<dy>,<n>,<ms>` | `n` such scrolls of `dy`, one every `ms` |
//! | `key:<chord>` | a chord as the panel script writes it (`key:down`, `key:cmd-c`) |
//! | `move:<x>,<y>` / `click:<x>,<y>` / `cmd-click:<x>,<y>` | the pointer there, and a click |
//! | `drag:<x0>,<y0>,<x1>,<y1>` | a press, a move and a release |
//! | `minimize` / `restore` | the window to the Dock and back |

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::ui::components::panel_script::{parse_chord, ChordKey, Step as PanelStep};
use marspot_term::input_core::Modifiers;

/// Between steps, long enough for one to be handled before the next.
const STEP_GAP: Duration = Duration::from_millis(300);
/// How often an `until` step looks again.
const UNTIL_GAP: Duration = Duration::from_millis(100);
/// The most steps a script may expand to.
const MAX_STEPS: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Until(String),
    Wait(u64),
    Scroll(f64, f64),
    Key(ChordKey, Modifiers),
    Move(f64, f64),
    Click(f64, f64, bool),
    Drag(f64, f64, f64, f64),
    Minimize(bool),
}

fn nums(text: &str, n: usize) -> Option<Vec<f64>> {
    let v: Vec<f64> = text.split(',').map(|t| t.trim().parse().ok()).collect::<Option<_>>()?;
    (v.len() == n && v.iter().all(|x| x.is_finite())).then_some(v)
}

pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    for text in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let bad = || format!("unknown step `{text}`");
        let (verb, arg) = text.split_once(':').unwrap_or((text, ""));
        match verb {
            "until" if !arg.is_empty() && !arg.contains('/') => out.push(Step::Until(arg.to_owned())),
            "wait" => out.push(Step::Wait(arg.parse().map_err(|_| bad())?)),
            "scroll" => {
                let v = nums(arg, 2).ok_or_else(bad)?;
                out.push(Step::Scroll(v[0], v[1]));
            }
            "wheel" => {
                let v = nums(arg, 3).ok_or_else(bad)?;
                if v[1] < 0.0 || v[1] as usize > MAX_STEPS / 2 || v[2] < 0.0 {
                    return Err(bad());
                }
                for _ in 0..v[1] as usize {
                    out.extend([Step::Scroll(0.0, v[0]), Step::Wait(v[2] as u64)]);
                }
            }
            "key" => match parse_chord(arg) {
                Some(PanelStep::Key(k, m)) => out.push(Step::Key(k, m)),
                _ => return Err(bad()),
            },
            "move" | "click" | "cmd-click" => {
                let v = nums(arg, 2).ok_or_else(bad)?;
                out.push(match verb {
                    "move" => Step::Move(v[0], v[1]),
                    _ => Step::Click(v[0], v[1], verb == "cmd-click"),
                });
            }
            "drag" => {
                let v = nums(arg, 4).ok_or_else(bad)?;
                out.push(Step::Drag(v[0], v[1], v[2], v[3]));
            }
            "minimize" | "restore" if arg.is_empty() => out.push(Step::Minimize(verb == "minimize")),
            _ => return Err(bad()),
        }
        if out.len() > MAX_STEPS {
            return Err("the script is too long".into());
        }
    }
    Ok(out)
}

/// A script being played, one step at a time.
pub struct Script {
    steps: VecDeque<Step>,
    next_at: Instant,
}

impl Script {
    /// The script the environment asks for, where dev seams are honoured.
    pub fn from_env() -> Option<Script> {
        let text = super::dev::dev_var("MARSPOT_DEV_WEB_SCRIPT")?;
        match parse(&text) {
            Ok(steps) => Some(Script { steps: steps.into(), next_at: Instant::now() }),
            Err(why) => {
                crate::lx_event!("DEV_WEB_SCRIPT_REJECTED", "the web pane script does not parse", why = why);
                None
            }
        }
    }

    pub fn finished(&self) -> bool {
        self.steps.is_empty()
    }

    /// The next step, when it is due: an `until` whose file is not there
    /// yet is not, nor is anything after it.
    pub fn next(&mut self, now: Instant) -> Option<Step> {
        if now < self.next_at {
            return None;
        }
        if let Some(Step::Until(name)) = self.steps.front()
            && !marspot_term::paths::state_root().join(name).exists()
        {
            self.next_at = now + UNTIL_GAP;
            return None;
        }
        let step = self.steps.pop_front()?;
        self.next_at = now + match step {
            Step::Wait(ms) => Duration::from_millis(ms),
            Step::Until(_) => Duration::ZERO,
            // a wheel's scrolls are paced by its waits alone
            _ if matches!(self.steps.front(), Some(Step::Wait(_))) => Duration::ZERO,
            _ => STEP_GAP,
        };
        Some(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_pane_dev_script_parses_and_expands_the_wheel() {
        let s = parse("until:go; scroll:0,310; wheel:40,2,33; key:cmd-c; cmd-click:20,20; drag:1,2,3,4; minimize").unwrap();
        assert_eq!(s[0], Step::Until("go".into()));
        assert_eq!(s[1], Step::Scroll(0.0, 310.0));
        assert_eq!(&s[2..6], &[Step::Scroll(0.0, 40.0), Step::Wait(33), Step::Scroll(0.0, 40.0), Step::Wait(33)]);
        assert!(matches!(s[6], Step::Key(ChordKey::Char('c'), m) if m.super_));
        assert_eq!(s[7], Step::Click(20.0, 20.0, true));
        assert_eq!(s[8], Step::Drag(1.0, 2.0, 3.0, 4.0));
        assert_eq!(&s[9..], &[Step::Minimize(true)]);
        for bad in ["until:a/b", "scroll:1", "wheel:1,99999,1", "click:x,1", "key:hyper-a", "fly"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
