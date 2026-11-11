//! Which language the interface is in.
//!
//! The same shape as the theme: the setting names a language or says
//! `system`, the shell reports what `system` resolves to, and the
//! language in force is one atomic to read.  It lives here, below the
//! interface, because the shell and the core both need it and neither
//! should need the other's drawing code to ask.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LangId {
    En = 0,
    Ja = 1,
    ZhHans = 2,
}

impl LangId {
    pub const ALL: [LangId; 3] = [LangId::En, LangId::Ja, LangId::ZhHans];

    /// The language's tag, as the settings file, the log and the
    /// system's own language lists write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
            Self::ZhHans => "zh-Hans",
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str().eq_ignore_ascii_case(v.trim()))
    }

    /// The value of the language setting: a tag, `system` for none,
    /// and anything else leaves what was there.
    pub fn parse_setting(v: &str, fallback: Option<Self>) -> Option<Self> {
        if v.trim().eq_ignore_ascii_case("system") {
            return None;
        }
        Self::parse(v).map_or(fallback, Some)
    }

    /// The language a system language tag asks for, if it is one of
    /// ours: `ja-JP` is Japanese, `zh-Hans-CN` and `zh-CN` simplified
    /// Chinese, `en-GB` English.  Traditional Chinese is not ours.
    pub fn of_system_tag(tag: &str) -> Option<Self> {
        let tag = tag.trim().to_ascii_lowercase().replace('_', "-");
        let mut parts = tag.split('-');
        match parts.next()? {
            "en" => Some(Self::En),
            "ja" => Some(Self::Ja),
            "zh" => match parts.next() {
                Some("hans" | "cn" | "sg") | None => Some(Self::ZhHans),
                Some(_) => None,
            },
            _ => None,
        }
    }

    /// The first language of the user's preferred list that is ours,
    /// or English when none is.
    pub fn of_preferred<'a>(tags: impl IntoIterator<Item = &'a str>) -> Self {
        tags.into_iter().find_map(Self::of_system_tag).unwrap_or(Self::En)
    }

    fn from_u8(v: u8) -> Self {
        Self::ALL.get(v as usize).copied().unwrap_or(Self::En)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(LangId::En as u8);
/// Counts changes of the language in force.
static VERSION: AtomicU32 = AtomicU32::new(0);
/// What the setting says: a language, or `FOLLOW_SYSTEM`.
static CHOICE: AtomicU8 = AtomicU8::new(FOLLOW_SYSTEM);
/// What `system` resolves to, as the shell last reported it.
static SYSTEM: AtomicU8 = AtomicU8::new(LangId::En as u8);
const FOLLOW_SYSTEM: u8 = u8::MAX;

pub fn current_id() -> LangId {
    LangId::from_u8(CURRENT.load(Ordering::Relaxed))
}

pub fn version() -> u32 {
    VERSION.load(Ordering::Relaxed)
}

/// Whether the language in force comes from the system's language
/// list rather than from one the user named.
pub fn follows_system() -> bool {
    CHOICE.load(Ordering::Relaxed) == FOLLOW_SYSTEM
}

fn resolve() {
    let choice = CHOICE.load(Ordering::Relaxed);
    let next = if choice == FOLLOW_SYSTEM { SYSTEM.load(Ordering::Relaxed) } else { choice };
    if CURRENT.swap(next, Ordering::Relaxed) != next {
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Put the setting in force: a named language, or `None` to follow
/// the system.
pub fn set_choice(lang: Option<LangId>) {
    CHOICE.store(lang.map_or(FOLLOW_SYSTEM, |l| l as u8), Ordering::Relaxed);
    resolve();
}

/// Record what `system` resolves to.  It takes effect at once when
/// the setting follows the system, and is remembered when it does not.
pub fn set_system(lang: LangId) {
    SYSTEM.store(lang as u8, Ordering::Relaxed);
    resolve();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_tag_maps_to_the_language_it_asks_for() {
        for (tag, want) in [
            ("en", Some(LangId::En)),
            ("en-GB", Some(LangId::En)),
            ("ja", Some(LangId::Ja)),
            ("ja-JP", Some(LangId::Ja)),
            ("zh-Hans", Some(LangId::ZhHans)),
            ("zh-Hans-CN", Some(LangId::ZhHans)),
            ("zh_CN", Some(LangId::ZhHans)),
            ("zh-Hant-TW", None),
            ("zh-TW", None),
            ("ko-KR", None),
            ("", None),
        ] {
            assert_eq!(LangId::of_system_tag(tag), want, "{tag:?}");
        }
        assert_eq!(LangId::of_preferred(["ko-KR", "ja-JP", "en-US"]), LangId::Ja);
        assert_eq!(LangId::of_preferred(["zh-Hant-TW", "fr-FR"]), LangId::En);
        assert_eq!(LangId::of_preferred([]), LangId::En);
    }

    #[test]
    fn the_setting_reads_a_tag_or_system() {
        assert_eq!(LangId::parse_setting("ja", None), Some(LangId::Ja));
        assert_eq!(LangId::parse_setting("ZH-HANS", None), Some(LangId::ZhHans));
        assert_eq!(LangId::parse_setting("system", Some(LangId::Ja)), None);
        assert_eq!(LangId::parse_setting("klingon", Some(LangId::Ja)), Some(LangId::Ja));
    }
}
