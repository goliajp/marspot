//! Every string a user reads, in one place.
//!
//! S7-01. v1 is a commercial product for a Japanese company, so the text
//! has to be translatable; today it is English literals spread through the
//! components that draw them, which is not a thing a translator can be
//! handed.
//!
//! ## Why a `match` and not a table
//!
//! The catalogue is an exhaustive `match` per language. Adding a [`Msg`]
//! and forgetting its text is then a compile error, which is what parallel
//! arrays indexed by `as usize` cannot give — those drift silently, and the
//! drift shows up as the wrong sentence under a label. It costs nothing at
//! run time: a match over a contiguous enum is a jump table, and every arm
//! returns a `&'static str` already in the binary. Nothing here allocates,
//! which matters because these are read while drawing a frame.
//!
//! ## How a language is chosen
//!
//! A language is a [`Lang`]: two functions, one for the fixed sentences
//! and one for the sentences built around a value. [`t`] and [`write`] go
//! through one process-wide pointer to the language in force, and that
//! pointer starts at English. A binary that never installs another
//! language therefore never names another language's table, and the
//! linker leaves it out — which is what keeps the single-session
//! companion and the single-process build at their size.
//!
//! ## Sentences built around a value
//!
//! [`Arg`] carries the values, typed, and the language decides the
//! sentence: where the number goes, whether there is a plural, what the
//! lead word is. A call site never glues a translated word to a value,
//! because the order it would glue them in is English.
//!
//! ## Why `const` tables hold `Msg` and not `&str`
//!
//! The specs these came from are `const` (`settings_modal::SECTIONS`), and
//! a lookup that depends on a running program's locale cannot be called in
//! a const initialiser. So the table holds the id and the view resolves it
//! where it draws. That also puts every resolution at one depth, which is
//! where a locale would be read from.

use std::fmt;
use std::sync::atomic::{AtomicPtr, Ordering};

use marspot_term::language::{self, LangId};

mod en;
mod ja;
mod zh_hans;

/// A string a user reads.
///
/// One variant per distinct sentence, not per place it appears: two labels
/// that happen to read the same today are one `Msg` only if they would
/// still be the same sentence in another language. "Off" as a dimming
/// level and "Off" as a switch position are not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Msg {
    // Settings — section headings
    SettingsIdleReclamation,
    SettingsAppearance,
    SettingsClaudeCode,
    SettingsScrolling,
    // Settings — row labels
    ReclaimIdlePanes,
    IdleFor,
    WarmUpOnReturn,
    Theme,
    DimOtherPanes,
    CircledDigitsWide,
    ClaudeCodeReportsModel,
    WheelSpeed,
    // Settings — the cost line under each row
    CostReclaimIdlePanes,
    CostIdleFor,
    CostWarmUpOnReturn,
    CostTheme,
    CostDimOtherPanes,
    CostCircledDigitsWide,
    CostClaudeCodeReportsModel,
    CostWheelSpeed,
    // Pane context menu
    MenuCopy,
    MenuPaste,
    MenuClearScrollback,
    MenuClosePane,
    // Commands — as the menu bar and the shortcuts panel name them.  Copy
    // and paste are the context menu's two, the same sentences
    CmdAbout,
    CmdSettings,
    CmdHide,
    CmdHideOthers,
    CmdShowAll,
    CmdQuit,
    CmdNewWindow,
    CmdNewPane,
    CmdClosePanel,
    CmdFind,
    CmdToggleSidebar,
    CmdLayout,
    CmdProcesses,
    CmdAccounts,
    CmdFullScreen,
    CmdMinimize,
    CmdPrevPane,
    CmdNextPane,
    CmdPane1,
    CmdPane2,
    CmdPane3,
    CmdPane4,
    CmdPane5,
    CmdPane6,
    CmdPane7,
    CmdPane8,
    CmdPane9,
    CmdPaneRange,
    CmdShortcuts,
    // The menu bar's menus
    MenuBarApp,
    MenuBarFile,
    MenuBarEdit,
    MenuBarView,
    MenuBarWindow,
    MenuBarHelp,
    // Panel titles.  The layout panel is titled by its command
    PanelShortcuts,
    PanelSettings,
    PanelProcesses,
    PanelAccounts,
    // Settings — how long a pane is idle before it is reclaimed
    Idle15m,
    Idle30m,
    Idle1h,
    Idle2h,
    IdleNever,
    // Settings — how far the other panes are dimmed
    DimOff,
    DimLight,
    DimNormal,
    DimDeep,
    // Settings — themes
    ThemeSystem,
    ThemeDark,
    ThemeLight,
    ThemeContrast,
    // Settings — wheel speeds
    ScrollSlow,
    ScrollNormal,
    ScrollFast,
    ScrollFaster,
    // Pane and title-strip context menus, beyond the four above
    MenuNewPane,
    MenuToggleSidebar,
    MenuOpenLayout,
    MenuMoveToNewWindow,
    // The menu on a link in a pane
    LinkCopyUuid,
    LinkOpenUrl,
    LinkCopyUrl,
    LinkOpenFile,
    LinkRevealFile,
    LinkCopyPath,
    LinkSendEmail,
    LinkCopyEmail,
    LinkOpenInBrowser,
    LinkCopyIp,
    // Layout panel
    LayoutGrid,
    LayoutArrangement,
    LayoutApply,
    LayoutColumns,
    LayoutRows,
    // Process monitor
    ProcessPanes,
    ProcessProcesses,
    ColPane,
    ColPids,
    ColCpu,
    ColRss,
    ColProcess,
    ColPid,
    // Agent accounts
    AccountsHeading,
    AccountsAvailability,
    AccountsEmpty,
    TimelineNow,
    StatusOk,
    StatusNear,
    StatusLimit,
    StatusNoKey,
    // What an empty pane says, in its grid
    PaneEmpty,
    // The supervisor's banner over a window
    BannerRestartsPaused,
    BannerRecovering,
    BannerCrashLoop,
    // Why a pane's badge would not switch accounts
    BadgeNoAccountFree,
    BadgeNoUsageData,
    BadgeModelUnknown,
    BadgeHeldByBgJob,
    // Settings — the interface language.  A language is named in itself,
    // whatever language the rest is in
    Language,
    CostLanguage,
    CostLanguageRelaunch,
    LangEnglish,
    LangJapanese,
    LangChinese,
}

/// A month, day and time of day, local. Written `M/D HH:mm` in every
/// language: all of them put the month first, and the cards it is drawn
/// in are too narrow for a spelled-out date.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MonthDayTime {
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

impl fmt::Display for MonthDayTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} {:02}:{:02}", self.month, self.day, self.hour, self.minute)
    }
}

/// A sentence built around a value.
///
/// The call site hands over the values and nothing else. Word order,
/// plurals and the words themselves are the language's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg<'a> {
    /// Context menu: move this pane to the window numbered so.
    MoveToWindow(usize),
    /// Layout panel footer: the grid's shape and how many panes it holds.
    PaneCount { cols: usize, rows: usize },
    /// Process monitor: which pane the right-hand table is showing.
    ProcessDetail { pane: &'a str, sid: u64, shell_pid: i32 },
    /// Account card: when its soonest window turns over.
    ResetAt(MonthDayTime),
    /// Accounts panel header: when the numbers were read.
    UpdatedAt(MonthDayTime),
    /// An empty pane whose session is on its way.
    SessionStarting(u64),
    /// An empty pane whose session did not come up.
    SessionUnavailable(u64),
    /// Badge menu: one account profile, marked when it is the pane's.
    ProfileRow { n: u8, current: bool },
    /// Badge menu: hand the conversation to another agent, named as its
    /// command and profile (`codex P2`), which no language translates.
    HandOffTo(&'a str),
    /// Badge menu: move this pane to another account profile.
    SwitchToProfile(u8),
}

/// One language: the fixed sentences and the ones built around a value.
pub struct Lang {
    pub text: fn(Msg) -> &'static str,
    pub write: for<'a> fn(Arg<'a>, &mut dyn fmt::Write) -> fmt::Result,
}

// starts at English and is only ever pointed at another `static`, so a
// binary that never installs a language links no table but this one
static CURRENT: AtomicPtr<Lang> = AtomicPtr::new(&en::LANG as *const Lang as *mut Lang);

#[inline]
fn lang() -> &'static Lang {
    // SAFETY: every value stored is the address of a `static Lang`
    unsafe { &*CURRENT.load(Ordering::Relaxed) }
}

/// One language's table.  Only this and [`follow_language`] name the
/// tables other than English.
pub fn table(id: LangId) -> &'static Lang {
    match id {
        LangId::En => &en::LANG,
        LangId::Ja => &ja::LANG,
        LangId::ZhHans => &zh_hans::LANG,
    }
}

/// Point [`t`] and [`write`] at the language now in force.  Called by
/// the processes that follow the language setting; one that never
/// calls it stays in English and links no other table.
pub fn follow_language() {
    CURRENT.store(table(language::current_id()) as *const Lang as *mut Lang, Ordering::Relaxed);
}

/// The text for `m` in the language the user reads.
#[inline]
pub fn t(m: Msg) -> &'static str {
    (lang().text)(m)
}

/// Writes the sentence for `a` into `out`, in the language the user
/// reads. `out` is the caller's, so a caller that keeps its buffer
/// allocates nothing here.
pub fn write(out: &mut impl fmt::Write, a: Arg<'_>) -> fmt::Result {
    (lang().write)(a, out)
}

/// The sentence for `a` as a new `String`, for the places that build
/// their text per event rather than per frame.
pub fn text(a: Arg<'_>) -> String {
    let mut s = String::new();
    // writing into a `String` cannot fail
    let _ = write(&mut s, a);
    s
}

/// Every `Msg`, for the tests and for whatever hands a translator the list.
///
/// Kept beside the enum on purpose: a variant added without being listed
/// here is caught by `every_message_is_listed`, which compares this against
/// the catalogue rather than trusting it.
pub const ALL: &[Msg] = &[
    Msg::SettingsIdleReclamation,
    Msg::SettingsAppearance,
    Msg::SettingsClaudeCode,
    Msg::SettingsScrolling,
    Msg::ReclaimIdlePanes,
    Msg::IdleFor,
    Msg::WarmUpOnReturn,
    Msg::Theme,
    Msg::DimOtherPanes,
    Msg::CircledDigitsWide,
    Msg::ClaudeCodeReportsModel,
    Msg::WheelSpeed,
    Msg::CostReclaimIdlePanes,
    Msg::CostIdleFor,
    Msg::CostWarmUpOnReturn,
    Msg::CostTheme,
    Msg::CostDimOtherPanes,
    Msg::CostCircledDigitsWide,
    Msg::CostClaudeCodeReportsModel,
    Msg::CostWheelSpeed,
    Msg::MenuCopy,
    Msg::MenuPaste,
    Msg::MenuClearScrollback,
    Msg::MenuClosePane,
    Msg::CmdAbout,
    Msg::CmdSettings,
    Msg::CmdHide,
    Msg::CmdHideOthers,
    Msg::CmdShowAll,
    Msg::CmdQuit,
    Msg::CmdNewWindow,
    Msg::CmdNewPane,
    Msg::CmdClosePanel,
    Msg::CmdFind,
    Msg::CmdToggleSidebar,
    Msg::CmdLayout,
    Msg::CmdProcesses,
    Msg::CmdAccounts,
    Msg::CmdFullScreen,
    Msg::CmdMinimize,
    Msg::CmdPrevPane,
    Msg::CmdNextPane,
    Msg::CmdPane1,
    Msg::CmdPane2,
    Msg::CmdPane3,
    Msg::CmdPane4,
    Msg::CmdPane5,
    Msg::CmdPane6,
    Msg::CmdPane7,
    Msg::CmdPane8,
    Msg::CmdPane9,
    Msg::CmdPaneRange,
    Msg::CmdShortcuts,
    Msg::MenuBarApp,
    Msg::MenuBarFile,
    Msg::MenuBarEdit,
    Msg::MenuBarView,
    Msg::MenuBarWindow,
    Msg::MenuBarHelp,
    Msg::PanelShortcuts,
    Msg::PanelSettings,
    Msg::PanelProcesses,
    Msg::PanelAccounts,
    Msg::Idle15m,
    Msg::Idle30m,
    Msg::Idle1h,
    Msg::Idle2h,
    Msg::IdleNever,
    Msg::DimOff,
    Msg::DimLight,
    Msg::DimNormal,
    Msg::DimDeep,
    Msg::ThemeSystem,
    Msg::ThemeDark,
    Msg::ThemeLight,
    Msg::ThemeContrast,
    Msg::ScrollSlow,
    Msg::ScrollNormal,
    Msg::ScrollFast,
    Msg::ScrollFaster,
    Msg::MenuNewPane,
    Msg::MenuToggleSidebar,
    Msg::MenuOpenLayout,
    Msg::MenuMoveToNewWindow,
    Msg::LinkCopyUuid,
    Msg::LinkOpenUrl,
    Msg::LinkCopyUrl,
    Msg::LinkOpenFile,
    Msg::LinkRevealFile,
    Msg::LinkCopyPath,
    Msg::LinkSendEmail,
    Msg::LinkCopyEmail,
    Msg::LinkOpenInBrowser,
    Msg::LinkCopyIp,
    Msg::LayoutGrid,
    Msg::LayoutArrangement,
    Msg::LayoutApply,
    Msg::LayoutColumns,
    Msg::LayoutRows,
    Msg::ProcessPanes,
    Msg::ProcessProcesses,
    Msg::ColPane,
    Msg::ColPids,
    Msg::ColCpu,
    Msg::ColRss,
    Msg::ColProcess,
    Msg::ColPid,
    Msg::AccountsHeading,
    Msg::AccountsAvailability,
    Msg::AccountsEmpty,
    Msg::TimelineNow,
    Msg::StatusOk,
    Msg::StatusNear,
    Msg::StatusLimit,
    Msg::StatusNoKey,
    Msg::PaneEmpty,
    Msg::BannerRestartsPaused,
    Msg::BannerRecovering,
    Msg::BannerCrashLoop,
    Msg::BadgeNoAccountFree,
    Msg::BadgeNoUsageData,
    Msg::BadgeModelUnknown,
    Msg::BadgeHeldByBgJob,
    Msg::Language,
    Msg::CostLanguage,
    Msg::CostLanguageRelaunch,
    Msg::LangEnglish,
    Msg::LangJapanese,
    Msg::LangChinese,
];
