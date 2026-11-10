//! The application menu: About, Hide, Quit.
//!
//! Without a main menu AppKit shows the app's name in the menu bar
//! with nothing under it, and Cmd-Q does nothing — the only ways out
//! were the Dock or closing every pane.  These are the items every Mac
//! app has, routed to AppKit's own actions; `terminate:` goes through
//! the delegate like a Dock quit does.
//!
//! Only this menu.  A key equivalent in the main menu is taken before
//! the view sees the key, so an Edit menu would take Cmd-C and Cmd-V
//! away from the panes, which handle them themselves.

use objc2::{sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

pub fn install(nsapp: &NSApplication, mtm: MainThreadMarker) {
    let item = |title: &str, action: objc2::runtime::Sel, key: &str| {
        // SAFETY: the action is one of AppKit's own, sent up the
        // responder chain with no target.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        }
    };
    let menu = NSMenu::new(mtm);
    menu.addItem(&item("About Marspot", sel!(orderFrontStandardAboutPanel:), ""));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item("Hide Marspot", sel!(hide:), "h"));
    let others = item("Hide Others", sel!(hideOtherApplications:), "h");
    others.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    menu.addItem(&others);
    menu.addItem(&item("Show All", sel!(unhideAllApplications:), ""));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item("Quit Marspot", sel!(terminate:), "q"));

    // the first item of the main menu is the application menu, whatever
    // its own title says
    let bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&menu));
    bar.addItem(&app_item);
    nsapp.setMainMenu(Some(&bar));
}
