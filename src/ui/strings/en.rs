//! English. The source language: these are the sentences the product was
//! written in, and a translation is checked against them.

use std::fmt;

use super::{Arg, Lang, Msg};

pub(super) static LANG: Lang = Lang { text, write };

fn text(m: Msg) -> &'static str {
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
        Msg::PanelSettings => "Settings",
        Msg::PanelProcesses => "Process monitor",
        Msg::PanelAccounts => "Agent accounts",

        Msg::Idle15m => "15m",
        Msg::Idle30m => "30m",
        Msg::Idle1h => "1h",
        Msg::Idle2h => "2h",
        Msg::IdleNever => "Never",

        Msg::DimOff => "Off",
        Msg::DimLight => "Light",
        Msg::DimNormal => "Normal",
        Msg::DimDeep => "Deep",

        Msg::ThemeSystem => "System",
        Msg::ThemeDark => "Dark",
        Msg::ThemeLight => "Light",
        Msg::ThemeContrast => "High contrast",

        Msg::ScrollSlow => "Slow",
        Msg::ScrollNormal => "Normal",
        Msg::ScrollFast => "Fast",
        Msg::ScrollFaster => "Faster",

        Msg::MenuNewPane => "New pane",
        Msg::MenuToggleSidebar => "Toggle sidebar",
        Msg::MenuOpenLayout => "Open layout…",
        Msg::MenuMoveToNewWindow => "Move to New Window",

        Msg::LinkCopyUuid => "Copy UUID",
        Msg::LinkOpenUrl => "Open URL",
        Msg::LinkCopyUrl => "Copy URL",
        Msg::LinkOpenFile => "Open file",
        Msg::LinkRevealFile => "Open containing folder",
        Msg::LinkCopyPath => "Copy path",
        Msg::LinkSendEmail => "Send email",
        Msg::LinkCopyEmail => "Copy email",
        Msg::LinkOpenInBrowser => "Open in browser",
        Msg::LinkCopyIp => "Copy IP",

        Msg::LayoutGrid => "Grid",
        Msg::LayoutArrangement => "Arrangement",
        Msg::LayoutApply => "Apply",
        Msg::LayoutColumns => "Columns",
        Msg::LayoutRows => "Rows",

        Msg::ProcessPanes => "Panes",
        Msg::ProcessProcesses => "Processes",
        Msg::ColPane => "Pane",
        Msg::ColPids => "pids",
        Msg::ColCpu => "CPU%",
        Msg::ColRss => "RSS",
        Msg::ColProcess => "Process",
        Msg::ColPid => "pid",

        Msg::AccountsHeading => "Accounts",
        Msg::AccountsAvailability => "Availability",
        Msg::AccountsEmpty => {
            "No usage read yet. Sign in to claude or codex in a pane; this fills in within a \
             few minutes."
        }
        Msg::TimelineNow => "NOW",
        Msg::StatusOk => "ok",
        Msg::StatusNear => "near",
        Msg::StatusLimit => "limit",
        Msg::StatusNoKey => "no key",

        Msg::PaneEmpty => "empty — click to start a shell",

        Msg::BannerRestartsPaused => "Marspot's core keeps stopping — it will retry on its own",
        Msg::BannerRecovering => "Marspot is recovering…",
        Msg::BannerCrashLoop => {
            "Marspot crashed repeatedly — safe mode (sessions kept, nothing new spawned)"
        }

        Msg::BadgeNoAccountFree => "⚠ no account free",
        Msg::BadgeNoUsageData => "⚠ no usage data",
        Msg::BadgeModelUnknown => "⚠ model unknown",
        Msg::BadgeHeldByBgJob => "⚠ held by bg job",

        Msg::Language => "Language",
        Msg::CostLanguage => "applies at once to Marspot's own panels and menus",
        Msg::CostLanguageRelaunch => "items the system provides switch after Marspot is relaunched",
        Msg::LangEnglish => "English",
        Msg::LangJapanese => "日本語",
        Msg::LangChinese => "简体中文",
    }
}

fn write(a: Arg<'_>, out: &mut dyn fmt::Write) -> fmt::Result {
    match a {
        Arg::MoveToWindow(n) => write!(out, "Move to Window {n}"),
        Arg::PaneCount { cols, rows } => {
            let total = cols * rows;
            let plural = if total == 1 { "" } else { "s" };
            write!(out, "{cols} × {rows} = {total} pane{plural}")
        }
        Arg::ProcessDetail { pane, sid, shell_pid } => {
            write!(out, "{pane} · sid {sid} · shell pid {shell_pid}")
        }
        Arg::ResetAt(at) => write!(out, "reset: {at}"),
        Arg::UpdatedAt(at) => write!(out, "updated {at}"),
        Arg::SessionStarting(id) => write!(out, "session {id} starting…"),
        Arg::SessionUnavailable(id) => {
            write!(out, "session {id} unavailable — press any key to retry")
        }
        Arg::ProfileRow { n, current: true } => write!(out, "● profile {n}"),
        Arg::ProfileRow { n, current: false } => write!(out, "   profile {n}"),
        Arg::HandOffTo(target) => write!(out, "hand off to {target}"),
        Arg::SwitchToProfile(n) => write!(out, "switch to P{n}"),
    }
}
