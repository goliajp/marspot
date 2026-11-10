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
//! ## Why there is no locale switch yet
//!
//! There is one language. A `Locale` enum, a stored preference and a
//! dispatch would be machinery for a second translation that does not
//! exist, and the project's rule is not to build that ahead of need.
//!
//! Adding one is: a second function with the same exhaustive `match`, and
//! `t` dispatching on the stored locale. The compiler then names every
//! string the new language is missing — which is the useful half of a
//! translation workflow, and the reason the catalogue is shaped this way.
//!
//! ## What is not here yet
//!
//! `Control::Segmented`'s option labels. Two of those arrays are prose
//! ("Off / Light / Normal / Deep", "Slow / Normal / Fast / Faster") and
//! belong here, but the third is durations ("15m / 30m / 1h / 2h"), which
//! is not a string to translate -- it is a number and a unit to format,
//! which is S7-03. Moving the control to carry `Msg` touches where it
//! draws, so it is the next slice rather than a half-done one here.
//!
//! ## Why `const` tables hold `Msg` and not `&str`
//!
//! The specs these came from are `const` (`settings_modal::SECTIONS`), and
//! a lookup that depends on a running program's locale cannot be called in
//! a const initialiser. So the table holds the id and the view resolves it
//! where it draws. That also puts every resolution at one depth, which is
//! where a locale would be read from.

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
    // Panel titles
    PanelShortcuts,
}

/// The text for `m` in the language the user reads.
#[inline]
pub fn t(m: Msg) -> &'static str {
    en(m)
}

/// English. The source language: these are the sentences the product was
/// written in, and a translation is checked against them.
fn en(m: Msg) -> &'static str {
    match m {
        Msg::SettingsIdleReclamation => "Idle reclamation",
        Msg::SettingsAppearance => "Appearance",
        Msg::SettingsClaudeCode => "Claude Code",
        Msg::SettingsScrolling => "Scrolling",

        Msg::ReclaimIdlePanes => "Reclaim idle claude panes",
        Msg::IdleFor => "Idle for",
        Msg::WarmUpOnReturn => "Warm up on return",
        Msg::Theme => "Theme",
        Msg::DimOtherPanes => "Dim the panes you are not in",
        Msg::CircledDigitsWide => "Circled digits take two cells",
        Msg::ClaudeCodeReportsModel => "Let Claude Code report its model",
        Msg::WheelSpeed => "Wheel speed",

        Msg::CostReclaimIdlePanes => "coming back to one costs ~3s while its session reloads",
        Msg::CostIdleFor => "measured by the session transcript's age, not terminal quiet",
        Msg::CostWarmUpOnReturn => "wakes parked panes one per second as you come back",
        Msg::CostTheme => "a program that asked for the old colours keeps them until it asks again",
        Msg::CostDimOtherPanes => {
            "deeper tells you at a glance what has drifted; too deep and you stop reading them"
        }
        Msg::CostCircledDigitsWide => {
            "sized like CJK, but moves the wrap point: text can strand a column early"
        }
        Msg::CostClaudeCodeReportsModel => {
            "adds a status-line entry to Claude Code's settings; yours, if any, is chained \
             and restored"
        }
        Msg::CostWheelSpeed => "faster gets there in fewer flicks and overshoots in one",


        Msg::MenuCopy => "Copy",
        Msg::MenuPaste => "Paste",
        Msg::MenuClearScrollback => "Clear scrollback",
        Msg::MenuClosePane => "Close pane",

        Msg::CmdAbout => "About Marspot",
        Msg::CmdSettings => "Settings…",
        Msg::CmdHide => "Hide Marspot",
        Msg::CmdHideOthers => "Hide Others",
        Msg::CmdShowAll => "Show All",
        Msg::CmdQuit => "Quit Marspot",
        Msg::CmdNewWindow => "New Window",
        Msg::CmdNewPane => "New Pane",
        Msg::CmdClosePanel => "Close Panel",
        Msg::CmdFind => "Find",
        Msg::CmdToggleSidebar => "Show or Hide Sidebar",
        Msg::CmdLayout => "Layout",
        Msg::CmdProcesses => "Process Monitor",
        Msg::CmdAccounts => "Agent Accounts",
        Msg::CmdFullScreen => "Enter or Exit Full Screen",
        Msg::CmdMinimize => "Minimize",
        Msg::CmdPrevPane => "Previous Pane",
        Msg::CmdNextPane => "Next Pane",
        Msg::CmdPane1 => "Pane 1",
        Msg::CmdPane2 => "Pane 2",
        Msg::CmdPane3 => "Pane 3",
        Msg::CmdPane4 => "Pane 4",
        Msg::CmdPane5 => "Pane 5",
        Msg::CmdPane6 => "Pane 6",
        Msg::CmdPane7 => "Pane 7",
        Msg::CmdPane8 => "Pane 8",
        Msg::CmdPane9 => "Pane 9",
        Msg::CmdPaneRange => "Pane 1 – 9",
        Msg::CmdShortcuts => "Keyboard Shortcuts",
        Msg::MenuBarApp => "Marspot",
        Msg::MenuBarFile => "File",
        Msg::MenuBarEdit => "Edit",
        Msg::MenuBarView => "View",
        Msg::MenuBarWindow => "Window",
        Msg::MenuBarHelp => "Help",
        Msg::PanelShortcuts => "Keyboard shortcuts",
    }
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
];
