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
        Msg::HelperStrip => "List helpers under their pane",
        Msg::CircledDigitsWide => "Circled digits take two cells",
        Msg::ClaudeCodeReportsModel => "Let Claude Code report its model",
        Msg::ClaudeCodeGuardModel => "Guard the default model all accounts share",
        Msg::WheelSpeed => "Wheel speed",

        Msg::CostReclaimIdlePanes => "coming back to one costs ~3s while its session reloads",
        Msg::CostIdleFor => "measured by the session transcript's age, not terminal quiet",
        Msg::CostWarmUpOnReturn => "wakes parked panes one per second as you come back",
        Msg::CostTheme => "a program that asked for the old colours keeps them until it asks again",
        Msg::CostDimOtherPanes => {
            "deeper tells you at a glance what has drifted; too deep and you stop reading them"
        }
        Msg::CostHelperStrip => "the pane is that much shorter while it has any",
        Msg::CostCircledDigitsWide => {
            "sized like CJK, but moves the wrap point: text can strand a column early"
        }
        Msg::CostClaudeCodeReportsModel => {
            "adds a status-line entry to Claude Code's settings; yours, if any, is chained \
             and restored"
        }
        Msg::CostClaudeCodeGuardModel => {
            "/model asks again before it is sent; a change nobody confirmed is put back"
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
        Msg::CmdPrevPrompt => "Jump to Previous Command",
        Msg::CmdNextPrompt => "Jump to Next Command",
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
        Msg::MenuBelongsTo => "Belongs to…",
        Msg::MenuBelongsToBlocked => "A pane with child panes can't belong to another pane",
        Msg::MenuMakeTopLevel => "Make top-level pane",
        Msg::MenuReplaceWith => "Replace with…",
        Msg::MenuNewShell => "New shell",
        Msg::CloseChildrenAsk => "Closing this pane also ends these child panes:",
        Msg::CloseChildrenConfirm => "Close All",
        Msg::CloseChildrenCancel => "Cancel",

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
        Msg::ProcessLeftGroup => "Left by closed panes",
        Msg::ProcessLeftDetail => "Still running after their pane closed; marspot never ends them itself",

        Msg::PanelHelpers => "Helpers",
        Msg::HelpersEmpty => "No helpers are running",
        Msg::HelperViewHere => "View here",
        Msg::HelperViewBack => "Back to this session",
        Msg::HelperEnded => "ended",
        Msg::HelperMoveToWindow => "Move to window",
        Msg::HelperNotMoved => "Not moved to a window",
        Msg::HelperClose => "Close",
        Msg::HelpersCloseAll => "Close all helpers of this session",
        Msg::HelpersDeny => "Block pane control from this directory",
        Msg::HelpersAllow => "Allow pane control from this directory",

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

        Msg::ModelGuardNotice => {
            "/model changes the default model of every Claude Code account on this Mac. \
             Press y to send it, Esc to keep editing"
        }
        Msg::ModelSendRefused => {
            "A program tried to send /model to Claude Code and it was not sent: only the \
             keyboard can change the default model of every account"
        }
        Msg::ClaudeSettingsChanged => "Claude Code's settings, shared by every account, changed",
        Msg::ModelUnset => "its own default",
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
        Msg::BadgeCodexTurnRunning => "⚠ turn running, not switched",
        Msg::BadgeCodexAgentRunning => "⚠ sub-agent running, not switched",
        Msg::BadgeCodexQueued => "⚠ messages queued, not switched",
        Msg::BadgeCodexCustomCommand => "⚠ custom command line, not switched",
        Msg::BadgeCodexStateUnknown => "⚠ session state unknown, not switched",
        Msg::BadgeCodexSwitching => "⚠ already switching",
        Msg::BadgeCodexNotTracked => "⚠ session not tracked yet, not switched",
        Msg::BadgeCodexTerminalBusy => "⚠ codex is at work, not switched",
        Msg::BadgeCodexWaitingRelease => "waiting for the old account to let go",
        Msg::BadgeCodexStillLoaded => "⚠ old account kept the session, not switched",

        Msg::Language => "Language",
        Msg::CostLanguage => "applies at once to Marspot's own panels and menus",
        Msg::FontFamily => "Font",
        Msg::CostFontFamily => "every pane re-wraps to the new cell size; full-screen programs redraw",
        Msg::FontSize => "Font size",
        Msg::CostFontSize => "larger is easier to read and fits fewer rows and columns in each pane",
        Msg::CostLanguageRelaunch => "items the system provides switch after Marspot is relaunched",
        Msg::LangEnglish => "English",
        Msg::LangJapanese => "日本語",
        Msg::LangChinese => "简体中文",

        Msg::SettingsAgentClis => "Agent CLIs",
        Msg::InstallClaude => "Install claude when it is missing",
        Msg::InstallCodex => "Install codex when it is missing",
        Msg::InstallGemini => "Install gemini when it is missing",
        Msg::InstallKimi => "Install kimi when it is missing",
        Msg::CostInstallCli => "downloads its vendor's build; marspot keeps it updated unless set to Don't install",
        Msg::ConsentAsk => "Ask",
        Msg::ConsentYes => "Install",
        Msg::ConsentNo => "Don't install",
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
        Arg::Helpers(n) => write!(out, "Helpers {n}"),
        Arg::HelpersOf(n) => write!(out, "helpers ({n})…"),
        Arg::ChildOf(code) => write!(out, " (child of #{code:04})"),
        Arg::MoreChildren(n) => write!(out, "…and {n} more"),
        Arg::RanFor { minutes: m @ 0..60 } => write!(out, "{m}m"),
        Arg::RanFor { minutes: m } => write!(out, "{}h {}m", m / 60, m % 60),
        Arg::ModelPutBack { to, back } => write!(
            out,
            "Claude Code's default model for every account was changed to {to} without confirmation; \
             it was put back to {back}"
        ),
        Arg::ModelNowIs(to) => write!(out, "Claude Code's default model for every account is now {to}"),
        Arg::InstallAsk { cli, place, megabytes } => write!(
            out,
            "Install {cli} into {place}? It is downloaded from its vendor and takes about {megabytes} MB. \
             Ctrl-Option-Y installs it, Ctrl-Option-N never installs or updates it; other keys go to the pane"
        ),
    }
}
