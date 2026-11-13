//! The shell's part in a language change.
//!
//! The shell is the process that can ask the system for the user's
//! language list, and the one that knows which language the items the
//! system provides — the Window menu's arrangement items, the About
//! panel, permission prompts — came up in.  It publishes both for the
//! core.  And it owns the text drawn outside the core's frames: the
//! menu bar, the status banner, and the badge text the plugins make.
//!
//! The items the system provides are in the language the process was
//! launched in and stay there; what this can do is make the next launch
//! come up in the chosen one, by naming it in the app's own defaults.
//!
//! The system's list is read on a supervisor pass, and the supervisor
//! rests while nothing happens.  The system says when the user's locale
//! changed, which a change of the preferred languages is; that asks for
//! a pass, so the change is followed when it is made and not when the
//! window is next touched.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};

use marspot::lx_event;
use marspot::lx_warn;
use marspot_term::language::{self, LangId};
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSBundle, NSCurrentLocaleDidChangeNotification, NSLocale, NSNotification,
    NSNotificationCenter, NSString, NSUserDefaults,
};

/// What was last published as the system's language; `u8::MAX` before
/// the first tick.
static PUBLISHED: AtomicU8 = AtomicU8::new(u8::MAX);
/// What the app's defaults were last made to say: a language, or
/// `u8::MAX - 1` for nothing; `u8::MAX` before the first tick.
static NAMED: AtomicU8 = AtomicU8::new(u8::MAX);
const NOTHING_NAMED: u8 = u8::MAX - 1;

thread_local! {
    /// The notification center holds its observers unretained.
    static WATCH: RefCell<Option<Retained<LocaleWatch>>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The one ivar is a function pointer; no Drop logic.
    #[unsafe(super(NSObject))]
    #[name = "MarspotLocaleWatch"]
    #[ivars = fn()]
    struct LocaleWatch;

    impl LocaleWatch {
        #[unsafe(method(changed:))]
        fn changed(&self, _note: &NSNotification) {
            (self.ivars())();
        }
    }
);

/// Call `on_change` whenever the system says the user's locale changed,
/// on the thread the notification is posted on.  One watch a thread: a
/// second call replaces the first.
fn watch(on_change: fn()) {
    unwatch();
    let this = LocaleWatch::alloc().set_ivars(on_change);
    // SAFETY: NSObject's `init`, on a freshly allocated subclass.
    let watch: Retained<LocaleWatch> = unsafe { msg_send![super(this), init] };
    // SAFETY: the observer is kept until `unwatch` removes it, and its
    // selector takes the notification.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &watch,
            sel!(changed:),
            Some(NSCurrentLocaleDidChangeNotification),
            None,
        );
    }
    WATCH.with(|w| *w.borrow_mut() = Some(watch));
}

/// Stop watching: the app was put away, and the shell that stays for
/// its sessions has no text to bring to another language.
pub(super) fn unwatch() {
    if let Some(watch) = WATCH.with(|w| w.borrow_mut().take()) {
        // SAFETY: removes the observer registered in `watch`.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&watch) };
    }
}

fn sandboxed() -> bool {
    std::env::var_os("MARSPOT_DEV_SANDBOX").is_some()
}

/// The first of the user's preferred languages that is ours.
fn system_language() -> LangId {
    let tags: Vec<String> = NSLocale::preferredLanguages().iter().map(|s| s.to_string()).collect();
    LangId::of_preferred(tags.iter().map(String::as_str))
}

/// Sandbox only: a file that stands in for the system's language list,
/// so a test can change it without touching the machine's setting.
fn sandbox_system_language() -> Option<LangId> {
    let path = std::env::var_os("MARSPOT_DEV_SYSTEM_LANGUAGE")?;
    LangId::of_system_tag(&std::fs::read_to_string(path).ok()?)
}

/// The language the items the system provides were launched in.  It
/// does not change while the process lives.
fn launched_in() -> LangId {
    let tags: Vec<String> =
        NSBundle::mainBundle().preferredLocalizations().iter().map(|s| s.to_string()).collect();
    LangId::of_preferred(tags.iter().map(String::as_str))
}

/// Make the next launch come up in the language the setting names, or
/// in the system's when it names none.  The key is only read at launch.
/// Not in a sandbox: the defaults belong to the installed app.
fn name_for_next_launch(choice: Option<LangId>) {
    let want = choice.map_or(NOTHING_NAMED, |l| l as u8);
    if NAMED.swap(want, Ordering::Relaxed) == want || sandboxed() {
        return;
    }
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = NSString::from_str("AppleLanguages");
    match choice {
        Some(lang) => {
            let list = NSArray::from_retained_slice(&[NSString::from_str(lang.as_str())]);
            // SAFETY: an array of strings is a property-list value.
            unsafe { defaults.setObject_forKey(Some(&list), &key) };
        }
        None => defaults.removeObjectForKey(&key),
    }
}

/// Put the language in force before anything is titled: the setting,
/// and what `system` resolves to.  The menu bar is built once, at
/// launch, and has to be built in the language the user reads.  From
/// here on a change of the system's list asks for a pass.
pub(super) fn at_launch() {
    let _ = marspot::settings::get();
    language::set_system(sandboxed().then(sandbox_system_language).flatten().unwrap_or_else(system_language));
    marspot::ui::strings::follow_language();
    watch(crate::pump::knock);
}

impl super::super::ShellApp {
    /// Publish the system's language if it changed, then bring the
    /// shell's own text to the language in force if that changed.
    pub(super) fn follow_language(&mut self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        name_for_next_launch(marspot::settings::get().appearance_language);
        let system = sandboxed().then(sandbox_system_language).flatten().unwrap_or_else(system_language);
        if PUBLISHED.swap(system as u8, Ordering::Relaxed) != system as u8
            && let Err(e) = marspot_term::system_language::publish(system, launched_in())
        {
            lx_warn!("shell.system_language.publish_failed", &e.to_string());
        }

        if !marspot_term::system_language::changed("the menu bar, the banner and badge text follow the language") {
            return;
        }
        marspot::ui::strings::follow_language();
        marspot::app_menu::retitle();
        // a banner on screen is a raster of the old language's sentence
        self.banner_kind = None;
        if sandboxed() {
            let bar = marspot::app_menu::titles(&NSApplication::sharedApplication(mtm));
            let top: Vec<&str> = bar.iter().map(|(title, _)| title.as_str()).collect();
            let lang = language::current_id().as_str();
            lx_event!("MENU_BAR", "the menu bar as read back from the application", lang = lang, top = top.join(","));
            // a line a menu: one line holds only so much
            for (title, items) in &bar {
                lx_event!("MENU_BAR_ITEMS", "one menu's items", lang = lang, menu = title.as_str(), items = items.join("|"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static SEEN: AtomicUsize = AtomicUsize::new(0);

    fn seen() {
        SEEN.fetch_add(1, Ordering::SeqCst);
    }

    fn post(name: &objc2_foundation::NSNotificationName) {
        // SAFETY: a notification with no object and no user info.
        unsafe { NSNotificationCenter::defaultCenter().postNotificationName_object(name, None) };
    }

    /// The system's notification of a changed locale reaches the watch,
    /// once for each time it is posted, and nothing else posted to the
    /// same center does; after `unwatch` it reaches nothing.
    #[test]
    fn a_changed_locale_asks_once_and_only_while_watched() {
        // SAFETY: Foundation's own constant strings, valid for the process.
        let (locale, other) = unsafe { (NSCurrentLocaleDidChangeNotification, objc2_foundation::NSSystemClockDidChangeNotification) };
        post(locale);
        assert_eq!(SEEN.load(Ordering::SeqCst), 0, "nothing watches yet");

        watch(seen);
        post(locale);
        assert_eq!(SEEN.load(Ordering::SeqCst), 1);
        post(other);
        assert_eq!(SEEN.load(Ordering::SeqCst), 1, "another notification is not a changed locale");
        post(locale);
        assert_eq!(SEEN.load(Ordering::SeqCst), 2);

        // a second watch replaces the first: one call a change, not two
        watch(seen);
        post(locale);
        assert_eq!(SEEN.load(Ordering::SeqCst), 3);

        unwatch();
        post(locale);
        assert_eq!(SEEN.load(Ordering::SeqCst), 3, "put away: nothing is asked for");
    }
}
