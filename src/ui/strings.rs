//! Every string a user reads, in one place.
//!
//! v1 is a commercial product for a Japanese company, so the text has to
//! be translatable, by a translator handed this catalogue.
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

mod all;
mod arg;
mod en;
mod ja;
mod zh_hans;
pub use all::ALL;
pub use arg::{Arg, MonthDayTime};

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
    HelperStrip,
    ClaudeCodeReportsModel,
    ClaudeCodeGuardModel,
    WheelSpeed,
    // Settings — the cost line under each row
    CostReclaimIdlePanes,
    CostIdleFor,
    CostWarmUpOnReturn,
    CostTheme,
    CostDimOtherPanes,
    CostCircledDigitsWide,
    CostHelperStrip,
    CostClaudeCodeReportsModel,
    CostClaudeCodeGuardModel,
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
    CmdPrevPrompt,
    CmdNextPrompt,
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
    // Whose child a pane is, from a pane's menu
    MenuBelongsTo,
    MenuBelongsToBlocked,
    MenuMakeTopLevel,
    MenuReplaceWith,
    MenuNewShell,
    // Asked before closing a pane with child panes
    CloseChildrenAsk,
    CloseChildrenConfirm,
    CloseChildrenCancel,
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
    /// The process monitor's entry for what closed panes left running.
    ProcessLeftGroup,
    ProcessLeftDetail,
    // Helper panel
    PanelHelpers,
    HelpersEmpty,
    HelperViewHere,
    HelperViewBack,
    HelperEnded,
    HelperMoveToWindow,
    HelperNotMoved,
    HelperClose,
    HelpersCloseAll,
    HelpersDeny,
    HelpersAllow,
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
    // Over a Claude Code pane: /model would change every account's default model
    ModelGuardNotice,
    ModelSendRefused,
    // The settings file every Claude Code account shares, as the shell watches it
    ClaudeSettingsChanged,
    /// What a default model with no `model` key in the file is called.
    ModelUnset,
    // The supervisor's banner over a window
    BannerRestartsPaused,
    BannerRecovering,
    BannerCrashLoop,
    // Why a pane's badge would not switch accounts
    BadgeNoAccountFree,
    BadgeNoUsageData,
    BadgeModelUnknown,
    BadgeHeldByBgJob,
    BadgeCodexTurnRunning,
    BadgeCodexAgentRunning,
    BadgeCodexQueued,
    BadgeCodexCustomCommand,
    BadgeCodexStateUnknown,
    BadgeCodexSwitching,
    BadgeCodexNotTracked,
    BadgeCodexTerminalBusy,
    BadgeCodexWaitingRelease,
    BadgeCodexStillLoaded,
    // Settings — the interface language.  A language is named in itself,
    // whatever language the rest is in
    Language,
    CostLanguage,
    CostLanguageRelaunch,
    LangEnglish,
    LangJapanese,
    LangChinese,
    // Settings — the terminal font
    FontFamily,
    CostFontFamily,
    FontSize,
    CostFontSize,
    // Settings — whether each agent CLI found missing is installed
    SettingsAgentClis,
    InstallClaude,
    InstallCodex,
    InstallGemini,
    InstallKimi,
    CostInstallCli,
    ConsentAsk,
    ConsentYes,
    ConsentNo,
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
