//! The menu bar, built from the command table.
//!
//! A key equivalent in the main menu is taken before the view sees the
//! key, so every chord listed here stops arriving as a key press once
//! the menu exists.  AppKit's own actions (hide, quit, minimise, full
//! screen) are meant to be taken that way.  Every other item replays
//! its chord as the key press the menu swallowed, into the window that
//! would have received it, so choosing an item and pressing its key
//! run the same code from there on.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

use crate::commands::{self, Command, Menu, Row, Runs, TABLE};
use crate::ui::strings::t;

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - Menu actions are sent on the main thread.
    // - No ivars and no Drop logic.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotMenuTarget"]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        /// The item's tag is its row in the command table.
        #[unsafe(method(runCommand:))]
        fn run_command(&self, sender: &NSMenuItem) {
            let Some(row) = usize::try_from(sender.tag()).ok().and_then(|i| TABLE.get(i)) else {
                return;
            };
            let Some(chord) = row.chord else { return };
            if crate::app::key_from_menu(chord.key_event(), chord.modifiers()) {
                return;
            }
            // A window that is not a terminal — the About panel — has
            // no panel to close; Cmd-W there closes the window.
            if row.command == Command::ClosePanel
                && let Some(window) = NSApplication::sharedApplication(self.mtm()).keyWindow()
            {
                window.performClose(None);
            }
        }
    }
);

fn key_mask(mods: u8) -> NSEventModifierFlags {
    let mut mask = NSEventModifierFlags::empty();
    for (bit, flag) in [
        (commands::SHIFT, NSEventModifierFlags::Shift),
        (commands::CTRL, NSEventModifierFlags::Control),
        (commands::OPT, NSEventModifierFlags::Option),
        (commands::CMD, NSEventModifierFlags::Command),
    ] {
        if mods & bit != 0 {
            mask |= flag;
        }
    }
    mask
}

fn item(index: usize, row: &Row, target: &MenuTarget, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let action = match row.runs {
        Runs::System { selector, .. } => {
            let name = std::ffi::CString::new(selector).expect("a selector has no NUL in it");
            Sel::register(&name)
        }
        Runs::Shell | Runs::Core => sel!(runCommand:),
    };
    let key = match row.chord.map(|c| c.key) {
        Some(commands::Key::Char(c)) => c.to_string(),
        Some(commands::Key::Esc) | None => String::new(),
    };
    // SAFETY: the action is AppKit's own, sent up the responder chain
    // with no target, or `runCommand:` on the target set below.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(t(row.title)),
            Some(action),
            &NSString::from_str(&key),
        )
    };
    if let Some(chord) = row.chord {
        item.setKeyEquivalentModifierMask(key_mask(chord.mods));
    }
    if !matches!(row.runs, Runs::System { .. }) {
        // SAFETY: the target is never released (see `install`).
        let target: &objc2::runtime::AnyObject = target;
        unsafe { item.setTarget(Some(target)) };
        item.setTag(index as isize);
    }
    item
}

pub fn install(nsapp: &NSApplication, mtm: MainThreadMarker) {
    let target: Retained<MenuTarget> = {
        let this = MenuTarget::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        unsafe { msg_send![super(this), init] }
    };
    let bar = NSMenu::new(mtm);
    for which in Menu::ALL {
        let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(t(which.title())));
        for (index, row) in TABLE.iter().enumerate().filter(|(_, r)| r.menu == which) {
            if row.separated {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            menu.addItem(&item(index, row, &target, mtm));
        }
        // The first item of the main menu is the application menu,
        // whatever its own title says.
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
        match which {
            Menu::Window => nsapp.setWindowsMenu(Some(&menu)),
            Menu::Help => nsapp.setHelpMenu(Some(&menu)),
            Menu::App | Menu::File | Menu::Edit | Menu::View => {}
        }
    }
    nsapp.setMainMenu(Some(&bar));
    // A menu item holds its target weakly, and the menu lives as long
    // as the process does.
    std::mem::forget(target);
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::ClassType;
    use objc2_app_kit::NSWindow;

    /// A system row names an AppKit action by its selector, which the
    /// compiler cannot check: a misspelt one is a menu item that is
    /// permanently greyed out.
    #[test]
    fn every_system_action_is_one_its_responder_answers() {
        let mut checked = 0;
        for row in TABLE {
            let Runs::System { selector, on } = row.runs else { continue };
            let sel = Sel::register(&std::ffi::CString::new(selector).unwrap());
            let class = match on {
                commands::Responder::App => NSApplication::class(),
                commands::Responder::Window => NSWindow::class(),
            };
            assert!(class.instance_method(sel).is_some(), "{:?}: nothing answers `{selector}`", row.command);
            checked += 1;
        }
        assert_eq!(checked, 7);
        let made_up = Sel::register(c"hideMarspotForGood:");
        assert!(NSApplication::class().instance_method(made_up).is_none(), "the check can fail");
    }

    #[test]
    fn the_key_mask_carries_each_modifier() {
        use NSEventModifierFlags as F;
        assert_eq!(key_mask(commands::CMD), F::Command);
        assert_eq!(key_mask(commands::SHIFT | commands::CMD), F::Shift | F::Command);
        assert_eq!(key_mask(commands::OPT | commands::CMD), F::Option | F::Command);
        assert_eq!(key_mask(commands::CTRL | commands::CMD), F::Control | F::Command);
        assert_eq!(key_mask(0), F::empty());
    }
}
