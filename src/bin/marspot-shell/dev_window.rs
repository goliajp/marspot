//! Sandbox seam: put the app's windows out of sight from a script.
//!
//! `MARSPOT_DEV_WINDOW_STATE=hidden[:secs]` or `minimised[:secs]`, read
//! only in a sandbox: three seconds after the first frame is on a
//! window the app is hidden, or its windows are minimised, and with
//! `secs` brought back that many seconds later.  A script cannot press
//! Cmd-H, and what a window nobody can see costs, and what it shows when
//! it is seen again, are otherwise untestable.  It arms nothing else:
//! the supervisor's clock rests as it does without it.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

pub(crate) const VAR: &str = "MARSPOT_DEV_WINDOW_STATE";

const AFTER_FIRST_FRAME: Duration = Duration::from_secs(3);

enum Step {
    /// Not yet read from the environment.
    Unread,
    Nothing,
    PutAway { minimise: bool, back_after: Option<Duration>, since: Instant },
    BringBack { minimise: bool, at: Instant },
}

thread_local! {
    static STEP: RefCell<Step> = const { RefCell::new(Step::Unread) };
}

fn read() -> Step {
    let asked = std::env::var(VAR).ok().filter(|_| std::env::var_os("MARSPOT_DEV_SANDBOX").is_some());
    let Some(asked) = asked else { return Step::Nothing };
    let (state, secs) = asked.split_once(':').map_or((asked.as_str(), None), |(s, n)| (s, n.parse().ok()));
    let minimise = match state {
        "hidden" => false,
        "minimised" => true,
        _ => return Step::Nothing,
    };
    Step::PutAway { minimise, back_after: secs.map(Duration::from_secs), since: Instant::now() }
}

fn put(minimise: bool, away: bool) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    match (minimise, away) {
        (false, true) => app.hide(None),
        (false, false) => app.unhide(None),
        (true, _) => {
            for w in app.windows().iter() {
                if away { w.miniaturize(None) } else { w.deminiaturize(None) }
            }
        }
    }
    marspot::lx_event!("DEV_WINDOW_STATE", "windows put out of sight or brought back (dev seam)", minimise = minimise, away = away);
}

/// Called on every pass once a window has shown its first frame.
pub(crate) fn pass() {
    STEP.with(|s| {
        let mut s = s.borrow_mut();
        let now = Instant::now();
        let next = match &*s {
            Step::Unread => read(),
            Step::PutAway { minimise, back_after, since } if now.duration_since(*since) >= AFTER_FIRST_FRAME => {
                put(*minimise, true);
                match back_after {
                    Some(d) => Step::BringBack { minimise: *minimise, at: now + *d },
                    None => Step::Nothing,
                }
            }
            Step::BringBack { minimise, at } if now >= *at => {
                put(*minimise, false);
                Step::Nothing
            }
            _ => return,
        };
        *s = next;
    });
}
