//! A script of menu-bar choices, played into a sandboxed app.
//!
//! Nothing outside the app can pull a menu down on a machine nobody is
//! logged in at the keyboard of, so the app chooses its own items: each
//! step finds the item in the application's installed main menu and
//! has AppKit perform it, which sends the item's action to its target
//! as a choice by pointer would.  The pointer itself, the menu opening
//! and its tracking are not exercised.
//!
//! `MARSPOT_DEV_MENU` holds the script and is read only when
//! `MARSPOT_DEV_SANDBOX` is set.  Steps are separated by `;`: a command
//! as `Command` prints (`Settings`, `FocusPane(1)`), or `wait:<ms>`.
//! Before the first step the installed menu is held to the command
//! table and the result logged as `DEV_MENU_AUDIT`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};

use crate::commands::{Row, Runs, TABLE};
use crate::lx_event;
use crate::ui::strings::t;

/// After the menu is installed: restored panes land seconds later.
const START_DELAY: f64 = 4.0;
/// Between steps, for the core to act on one before the next.
const STEP_GAP: f64 = 0.4;

thread_local! {
    static STEPS: RefCell<Option<VecDeque<String>>> = const { RefCell::new(None) };
    static AUDITED: Cell<bool> = const { Cell::new(false) };
}

fn schedule(target: &AnyObject, delay: f64) {
    // SAFETY: the target is the menu's, never released, and answers
    // `playMenuScript:`; a nil argument is what it is declared with.
    unsafe {
        let _: () = objc2::msg_send![
            target,
            performSelector: sel!(playMenuScript:),
            withObject: std::ptr::null::<AnyObject>(),
            afterDelay: delay,
        ];
    }
}

/// Read the script the environment asks for, in a sandbox only, and
/// have `target` sent `playMenuScript:` for its first step.
pub(crate) fn start(target: &AnyObject) {
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_none() {
        return;
    }
    let Ok(text) = std::env::var("MARSPOT_DEV_MENU") else { return };
    let steps = text.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    STEPS.set(Some(steps));
    schedule(target, START_DELAY);
}

/// Is a script being played?  Its choices are made with nobody at the
/// machine, where the app may not be the active one.
pub(crate) fn playing() -> bool {
    STEPS.with_borrow(Option::is_some)
}

/// Every item of the installed menu bar, with the menu it is in.
fn items(nsapp: &NSApplication) -> Vec<(Retained<NSMenu>, isize, Retained<NSMenuItem>)> {
    let Some(bar) = nsapp.mainMenu() else { return Vec::new() };
    let mut out = Vec::new();
    for menu in bar.itemArray().iter().filter_map(|holder| holder.submenu()) {
        for (at, item) in menu.itemArray().iter().enumerate() {
            out.push((menu.clone(), at as isize, item));
        }
    }
    out
}

/// Is `item` the one made for row `index` of the table?
fn is_row(item: &NSMenuItem, index: usize, row: &Row) -> bool {
    let action = item.action();
    match row.runs {
        Runs::System { selector, .. } => action.is_some_and(|a| a.name().to_str() == Ok(selector)),
        Runs::Shell | Runs::Core => action == Some(sel!(runCommand:)) && item.tag() == index as isize,
    }
}

/// Hold the installed menu to the table: every row has exactly one
/// item a person can see, in its own menu and under its own title, and
/// no item that runs a command is without a row.  Hidden items with a
/// row's action are AppKit's, and are listed, not counted.
fn audit(nsapp: &NSApplication) {
    let items = items(nsapp);
    let mut wrong: Vec<String> = Vec::new();
    // items AppKit keeps out of sight beside one of ours
    let mut unseen: Vec<String> = Vec::new();
    let mut commands = 0u32;
    for (index, row) in TABLE.iter().enumerate() {
        let (hidden, mine): (Vec<_>, Vec<_>) =
            items.iter().filter(|(_, _, item)| is_row(item, index, row)).partition(|(_, _, item)| item.isHidden());
        for (menu, at, item) in hidden {
            unseen.push(format!(
                "{:?} `{}` at {}[{at}] key `{}` mods {:#x} target {}",
                row.command,
                item.title(),
                menu.title(),
                item.keyEquivalent(),
                item.keyEquivalentModifierMask().0,
                item.target().map_or("nil".to_string(), |t| t.class().name().to_string_lossy().into_owned()),
            ));
        }
        match mine.as_slice() {
            [(menu, _, item)] => {
                if menu.title().to_string() != t(row.menu.title()) || item.title().to_string() != t(row.title) {
                    wrong.push(format!("{:?} is `{}` under `{}`", row.command, item.title(), menu.title()));
                }
                commands += !matches!(row.runs, Runs::System { .. }) as u32;
            }
            _ => wrong.push(format!("{:?} has {} items", row.command, mine.len())),
        }
    }
    let strays = items
        .iter()
        .filter(|(_, _, item)| item.action() == Some(sel!(runCommand:)))
        .filter(|(_, _, item)| {
            let row = usize::try_from(item.tag()).ok().and_then(|i| TABLE.get(i));
            row.is_none_or(|r| matches!(r.runs, Runs::System { .. }))
        })
        .count();
    lx_event!(
        "DEV_MENU_AUDIT",
        "the installed menu bar, held to the command table",
        rows = TABLE.len() as u32,
        commands = commands,
        wrong = wrong.join("; "),
        strays = strays as u32,
        hidden = unseen.join("; "),
        items = items.iter().filter(|(_, _, i)| !i.isSeparatorItem() && !i.isHidden()).count() as u32
    );
}

fn choose(name: &str, nsapp: &NSApplication) {
    let found = TABLE
        .iter()
        .enumerate()
        .find(|(_, row)| format!("{:?}", row.command) == name)
        .and_then(|(index, row)| {
            items(nsapp).into_iter().find(|(_, _, item)| is_row(item, index, row) && !item.isHidden())
        });
    let Some((menu, at, item)) = found else {
        lx_event!("DEV_MENU_STEP_FAILED", "no such item in the menu bar", command = name);
        return;
    };
    menu.update();
    lx_event!(
        "DEV_MENU_STEP",
        "choosing a menu item",
        command = name,
        title = item.title().to_string(),
        enabled = item.isEnabled() as u32,
        key_window = nsapp.keyWindow().is_some() as u32
    );
    menu.performActionForItemAtIndex(at);
}

/// Take the next step; `target` is sent `playMenuScript:` again for
/// the one after it.
pub(crate) fn play(target: &AnyObject, nsapp: &NSApplication) {
    if !AUDITED.replace(true) {
        // a choice goes to the key window, so there should be one
        nsapp.activate();
        if let Some(window) = nsapp.windows().iter().next() {
            window.makeKeyAndOrderFront(None);
        }
        audit(nsapp);
    }
    let Some(step) = STEPS.with_borrow_mut(|steps| steps.as_mut().map(VecDeque::pop_front)) else { return };
    let Some(step) = step else {
        STEPS.set(None);
        lx_event!("DEV_MENU_DONE", "the menu script has been played");
        return;
    };
    let gap = match step.strip_prefix("wait:") {
        Some(ms) => ms.parse::<f64>().map_or(STEP_GAP, |ms| ms / 1000.0),
        None => {
            choose(&step, nsapp);
            STEP_GAP
        }
    };
    schedule(target, gap);
}
