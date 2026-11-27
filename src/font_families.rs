//! The terminal font the settings name: which monospaced families this
//! Mac has, and how one of them is opened for the grid.
//!
//! `CTFontCreateWithName` never fails — a name it does not know comes
//! back as Helvetica — so a family is opened through the descriptors
//! of that family, and a family the machine lacks is reported as such
//! rather than drawn in a proportional face.
//!
//! Only faces registered on this Mac are matched.  A family macOS can
//! download on demand but has not (Osaka or Nanum Gothic on a fresh
//! install) is not installed: matching its name with
//! `CTFontDescriptorCreateMatchingFontDescriptors` waits on the font
//! asset service for minutes, while matching it inside a collection
//! answers at once with nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_text::font::{new_from_descriptor, CTFont};
use core_text::font_collection;
use core_text::font_descriptor::{
    self, kCTFontBoldTrait, kCTFontFamilyNameAttribute, kCTFontItalicTrait, kCTFontMonoSpaceTrait,
    CTFontDescriptor, TraitAccessors,
};

/// The four faces the grid draws with: regular, bold, italic,
/// bold-italic.  A style the family does not have is its regular face.
pub struct Faces {
    pub regular: CTFont,
    pub bold: CTFont,
    pub italic: CTFont,
    pub bold_italic: CTFont,
}

fn style_of(d: &CTFontDescriptor) -> u32 {
    d.traits().symbolic_traits() & (kCTFontBoldTrait | kCTFontItalicTrait)
}

fn is_mono(traits: u32) -> bool {
    traits & kCTFontMonoSpaceTrait != 0
}

/// The face the grid's regular style is drawn in: among the family's
/// monospaced faces, an unstyled one if there is any, then the one
/// whose weight and width are nearest normal, then by name, so the
/// choice never rests on the order CoreText lists the faces in.  A
/// family with a proportional face beside its monospaced one (Osaka
/// and Osaka-Mono) gives the monospaced one.
fn pick_regular(descs: &[CTFontDescriptor]) -> Option<&CTFontDescriptor> {
    let key = |d: &CTFontDescriptor| {
        let t = d.traits();
        (style_of(d) != 0, t.normalized_weight().abs() + t.normalized_width().abs(), d.font_name())
    };
    descs.iter().filter(|d| is_mono(d.traits().symbolic_traits())).min_by(|a, b| {
        let (a, b) = (key(a), key(b));
        a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2))
    })
}

/// The faces of `family` installed on this Mac, or `None` when it has
/// none, without ever waiting on a download.
fn installed_faces(family: &str) -> Option<Vec<CTFontDescriptor>> {
    // SAFETY: a constant CoreText exports, borrowed under the get rule
    let key = unsafe { CFString::wrap_under_get_rule(kCTFontFamilyNameAttribute) };
    let attrs = CFDictionary::from_CFType_pairs(&[(key, CFString::new(family).as_CFType())]);
    let wanted = CFArray::from_CFTypes(&[font_descriptor::new_from_attributes(&attrs)]);
    let descs = font_collection::new_from_descriptors(&wanted).get_descriptors()?;
    Some(descs.iter().map(|d| d.clone()).collect())
}

/// `family` at `point`, or `None` when the machine has no monospaced
/// face of that family — not installed (downloadable counts as not
/// installed until it has been downloaded), or proportional only (a name
/// written into the settings by hand, such as Helvetica) — so the
/// caller falls back to a terminal font.  A style the family lacks, or
/// has only as a proportional face, is the regular face.
pub fn open(family: &str, point: f64) -> Option<Faces> {
    let descs = installed_faces(family)?;
    let regular = new_from_descriptor(pick_regular(&descs)?, point);
    let mask = kCTFontBoldTrait | kCTFontItalicTrait;
    let variant = |traits: u32| {
        regular
            .clone_with_symbolic_traits(traits, mask)
            .filter(|f| is_mono(f.symbolic_traits()))
            .unwrap_or_else(|| regular.clone())
    };
    Some(Faces {
        bold: variant(kCTFontBoldTrait),
        italic: variant(kCTFontItalicTrait),
        bold_italic: variant(mask),
        regular,
    })
}

/// Every installed family with a monospaced face, sorted without
/// regard to case.  Families the system keeps for itself (names that
/// start with a dot) are left out: they cannot be chosen by name.
pub fn list_installed() -> Vec<String> {
    let Some(descs) = font_collection::create_for_all_families().get_descriptors() else {
        return Vec::new();
    };
    let mut names: Vec<String> = descs
        .iter()
        .filter(|d| is_mono(d.traits().symbolic_traits()))
        .map(|d| d.family_name())
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

fn store() -> &'static RwLock<Arc<[String]>> {
    static LIST: OnceLock<RwLock<Arc<[String]>>> = OnceLock::new();
    LIST.get_or_init(|| RwLock::new(Arc::from([])))
}

/// The families last listed: empty until the first [`refresh`] or
/// [`refresh_now`] has finished.
pub fn installed() -> Arc<[String]> {
    Arc::clone(&store().read().unwrap_or_else(|p| p.into_inner()))
}

static LISTINGS: AtomicU64 = AtomicU64::new(0);
static ON_LISTED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// How many listings have finished: a reader that saw a smaller number
/// has a stale [`installed`].
pub fn listings() -> u64 {
    LISTINGS.load(Ordering::Acquire)
}

/// Call `f` from the listing thread each time a [`refresh`] finishes,
/// so the thread that draws can look again without waiting on it.  The
/// first call sets it for the life of the process.
pub fn on_listed(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_LISTED.set(Box::new(f));
}

/// List the installed families now, on the calling thread.
pub fn refresh_now() {
    let fresh: Arc<[String]> = list_installed().into();
    *store().write().unwrap_or_else(|p| p.into_inner()) = fresh;
    LISTINGS.fetch_add(1, Ordering::AcqRel);
}

/// List the installed families again on a thread of its own, so a font
/// installed since the last look is offered.  The listing asks the font
/// server for every face on the machine (~140 ms measured on a runner
/// Mac), which the thread that draws must not wait for; a call while
/// one is still running does nothing.
pub fn refresh() {
    static LISTING: AtomicBool = AtomicBool::new(false);
    if LISTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("font-families".into()).spawn(|| {
        refresh_now();
        LISTING.store(false, Ordering::Release);
        if let Some(f) = ON_LISTED.get() {
            f();
        }
    });
    if spawned.is_err() {
        LISTING.store(false, Ordering::Release);
    }
}

/// The family after (`forward`) or before `current` in `list`, wrapping
/// at the ends.  A `current` that is not in the list (not installed, or
/// spelt differently by hand) steps to the first or the last.
pub fn step<'a>(list: &'a [String], current: &str, forward: bool) -> Option<&'a str> {
    let n = list.len();
    if n == 0 {
        return None;
    }
    let at = list.iter().position(|f| f.eq_ignore_ascii_case(current));
    let i = match (at, forward) {
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
        (None, true) => 0,
        (None, false) => n - 1,
    };
    Some(list[i].as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stepping_wraps_and_a_family_not_listed_starts_at_an_end() {
        let l = names(&["Courier", "Menlo", "Monaco"]);
        assert_eq!(step(&l, "Menlo", true), Some("Monaco"));
        assert_eq!(step(&l, "Monaco", true), Some("Courier"), "wraps forward");
        assert_eq!(step(&l, "Courier", false), Some("Monaco"), "wraps back");
        assert_eq!(step(&l, "monaco", false), Some("Menlo"), "case is not a different family");
        assert_eq!(step(&l, "Fira Code", true), Some("Courier"));
        assert_eq!(step(&l, "Fira Code", false), Some("Monaco"));
        assert_eq!(step(&[], "Menlo", true), None);
    }

    /// The families macOS ships are listed, a proportional one is not,
    /// and the list has no repeats.  Prints how long the listing took,
    /// since it runs on the core's thread when the panel opens.
    #[test]
    fn the_shipped_monospaced_families_are_listed_and_no_others() {
        let t = std::time::Instant::now();
        let l = list_installed();
        eprintln!("listed {} monospaced families in {:?}", l.len(), t.elapsed());
        // Courier is fixed-pitch but does not say so in its traits
        for want in ["Menlo", "Monaco", "Courier New"] {
            assert!(l.iter().any(|f| f == want), "{want} missing from {l:?}");
        }
        assert!(!l.iter().any(|f| f == "Helvetica"), "a proportional family listed: {l:?}");
        let mut d = l.clone();
        d.dedup();
        assert_eq!(d, l);
    }

    /// A family that is installed opens in that family; one that is not
    /// is reported missing instead of coming back as Helvetica.
    #[test]
    fn a_missing_family_is_none_not_helvetica() {
        let f = open("Menlo", 12.0).expect("Menlo ships with macOS");
        assert_eq!(f.regular.family_name(), "Menlo");
        assert!(f.bold.family_name() == "Menlo" && f.bold.symbolic_traits() & kCTFontBoldTrait != 0);
        assert!(open("No Such Family 7f3a", 12.0).is_none());
    }

    /// `f()` on a thread of its own, failing the test if it has not
    /// answered within `secs`: a call that waits on the font asset
    /// service fails here instead of at the runner's timeout
    fn within<T: Send + 'static>(secs: u64, what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let t = std::time::Instant::now();
        let v = rx
            .recv_timeout(std::time::Duration::from_secs(secs))
            .unwrap_or_else(|_| panic!("{what} still waiting after {secs} s"));
        eprintln!("{what}: {:?}", t.elapsed());
        v
    }

    /// Only a monospaced face is opened: a proportional family named by
    /// hand is refused, and a family with both kinds gives its
    /// monospaced face whatever order CoreText lists them in.  Whether
    /// Osaka is here is asked the way `open` asks, since on a Mac that
    /// has not downloaded it other matching calls wait on the font
    /// asset service.
    #[test]
    fn only_a_monospaced_face_is_opened() {
        let helvetica = within(5, "open Helvetica", || open("Helvetica", 12.0).is_none());
        assert!(helvetica, "a proportional family is not drawn");
        let has_osaka = within(5, "faces of Osaka", || {
            installed_faces("Osaka").is_some_and(|d| d.iter().any(|d| d.font_name() == "Osaka-Mono"))
        });
        if !has_osaka {
            eprintln!("SKIPPED: Osaka-Mono is not installed on this Mac");
            return;
        }
        let f = open("Osaka", 12.0).expect("Osaka has a monospaced face");
        assert_eq!(f.regular.postscript_name(), "Osaka-Mono");
        for face in [&f.bold, &f.italic, &f.bold_italic] {
            assert!(is_mono(face.symbolic_traits()), "{}", face.postscript_name());
        }
    }

    /// A family macOS downloads on demand, named in the settings on a
    /// Mac that has not downloaded it, is not installed: `open` answers
    /// `None` at once, so the grid falls back to Monaco.  Which of these
    /// are absent depends on the Mac; the families the system lists as
    /// available are the independent answer.
    #[test]
    fn a_downloadable_family_not_downloaded_is_none_at_once() {
        let available: Vec<String> = font_collection::create_for_all_families()
            .get_descriptors()
            .map(|d| d.iter().map(|d| d.family_name()).collect())
            .unwrap_or_default();
        let mut absent = 0;
        for family in ["Osaka", "Nanum Gothic", "PCMyungjo", "BiauKai"] {
            let here = available.iter().any(|f| f == family);
            let matched = within(5, &format!("faces of {family}"), move || installed_faces(family).is_some());
            assert_eq!(matched, here, "{family}: matched {matched}, listed as available {here}");
            if !here {
                absent += 1;
                assert!(within(5, &format!("open {family}"), move || open(family, 12.0).is_none()));
            }
        }
        eprintln!("{absent} of the downloadable families are absent on this Mac");
    }
}
