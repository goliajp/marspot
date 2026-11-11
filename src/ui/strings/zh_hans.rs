//! Simplified Chinese. Standard commands are worded as macOS words
//! them; labels take no full stop, descriptions are sentences and do;
//! a space separates Chinese from Latin letters and digits.

use std::fmt;

use super::{Arg, Lang, Msg};

pub(super) static LANG: Lang = Lang { text, write };

fn text(m: Msg) -> &'static str {
    match m {
        Msg::SettingsIdleReclamation => "闲置回收",
        Msg::SettingsAppearance => "外观",
        Msg::SettingsClaudeCode => "Claude Code",
        Msg::SettingsScrolling => "滚动",

        Msg::ReclaimIdlePanes => "回收闲置的 claude 窗格",
        Msg::IdleFor => "闲置时长",
        Msg::WarmUpOnReturn => "返回时预热",
        Msg::Theme => "主题",
        Msg::DimOtherPanes => "调暗非当前窗格",
        Msg::CircledDigitsWide => "带圈数字占两格",
        Msg::ClaudeCodeReportsModel => "让 Claude Code 报告所用模型",
        Msg::WheelSpeed => "滚轮速度",

        Msg::CostReclaimIdlePanes => "返回窗格时，重新载入会话约需 3 秒。",
        Msg::CostIdleFor => "按会话记录的更新时间判断，而非终端是否安静。",
        Msg::CostWarmUpOnReturn => "返回时，每秒唤醒一个已休眠的窗格。",
        Msg::CostTheme => "已取得旧颜色的程序会沿用旧颜色，直到再次查询。",
        Msg::CostDimOtherPanes => "调得越暗，越容易一眼看出差别；过暗则难以阅读。",
        Msg::CostCircledDigitsWide => "宽度与中日韩文字一致，但换行位置可能提前一列。",
        Msg::CostClaudeCodeReportsModel => {
            "在 Claude Code 的设置中添加状态行；已有的状态行会保留并可恢复。"
        }
        Msg::CostWheelSpeed => "越快越省滑动次数，但也越容易滚过头。",

        Msg::MenuCopy => "拷贝",
        Msg::MenuPaste => "粘贴",
        Msg::MenuClearScrollback => "清除回滚内容",
        Msg::MenuClosePane => "关闭窗格",

        Msg::CmdAbout => "关于 Marspot",
        Msg::CmdSettings => "设置…",
        Msg::CmdHide => "隐藏 Marspot",
        Msg::CmdHideOthers => "隐藏其他",
        Msg::CmdShowAll => "全部显示",
        Msg::CmdQuit => "退出 Marspot",
        Msg::CmdNewWindow => "新建窗口",
        Msg::CmdNewPane => "新建窗格",
        Msg::CmdClosePanel => "关闭面板",
        Msg::CmdFind => "查找",
        Msg::CmdToggleSidebar => "显示或隐藏边栏",
        Msg::CmdLayout => "布局",
        Msg::CmdProcesses => "进程监视器",
        Msg::CmdAccounts => "智能体账户",
        Msg::CmdFullScreen => "进入或退出全屏幕",
        Msg::CmdMinimize => "最小化",
        Msg::CmdPrevPane => "上一个窗格",
        Msg::CmdNextPane => "下一个窗格",
        Msg::CmdPane1 => "窗格 1",
        Msg::CmdPane2 => "窗格 2",
        Msg::CmdPane3 => "窗格 3",
        Msg::CmdPane4 => "窗格 4",
        Msg::CmdPane5 => "窗格 5",
        Msg::CmdPane6 => "窗格 6",
        Msg::CmdPane7 => "窗格 7",
        Msg::CmdPane8 => "窗格 8",
        Msg::CmdPane9 => "窗格 9",
        Msg::CmdPaneRange => "窗格 1 – 9",
        Msg::CmdShortcuts => "键盘快捷键",
        Msg::MenuBarApp => "Marspot",
        Msg::MenuBarFile => "文件",
        Msg::MenuBarEdit => "编辑",
        Msg::MenuBarView => "显示",
        Msg::MenuBarWindow => "窗口",
        Msg::MenuBarHelp => "帮助",
        Msg::PanelShortcuts => "键盘快捷键",
        Msg::PanelSettings => "设置",
        Msg::PanelProcesses => "进程监视器",
        Msg::PanelAccounts => "智能体账户",

        Msg::Idle15m => "15 分钟",
        Msg::Idle30m => "30 分钟",
        Msg::Idle1h => "1 小时",
        Msg::Idle2h => "2 小时",
        Msg::IdleNever => "永不",

        Msg::DimOff => "关闭",
        Msg::DimLight => "轻微",
        Msg::DimNormal => "标准",
        Msg::DimDeep => "明显",

        Msg::ThemeSystem => "跟随系统",
        Msg::ThemeDark => "深色",
        Msg::ThemeLight => "浅色",
        Msg::ThemeContrast => "高对比度",

        Msg::ScrollSlow => "慢",
        Msg::ScrollNormal => "标准",
        Msg::ScrollFast => "快",
        Msg::ScrollFaster => "更快",

        Msg::MenuNewPane => "新建窗格",
        Msg::MenuToggleSidebar => "显示或隐藏边栏",
        Msg::MenuOpenLayout => "打开布局…",
        Msg::MenuMoveToNewWindow => "移到新窗口",

        Msg::LinkCopyUuid => "拷贝 UUID",
        Msg::LinkOpenUrl => "打开 URL",
        Msg::LinkCopyUrl => "拷贝 URL",
        Msg::LinkOpenFile => "打开文件",
        Msg::LinkCopyPath => "拷贝路径",
        Msg::LinkSendEmail => "发送电子邮件",
        Msg::LinkCopyEmail => "拷贝电子邮件地址",
        Msg::LinkOpenInBrowser => "在浏览器中打开",
        Msg::LinkCopyIp => "拷贝 IP 地址",

        Msg::LayoutGrid => "网格",
        Msg::LayoutArrangement => "排列",
        Msg::LayoutApply => "应用",
        Msg::LayoutColumns => "列",
        Msg::LayoutRows => "行",

        Msg::ProcessPanes => "窗格",
        Msg::ProcessProcesses => "进程",
        Msg::ColPane => "窗格",
        Msg::ColPids => "进程数",
        Msg::ColCpu => "CPU%",
        Msg::ColRss => "RSS",
        Msg::ColProcess => "进程",
        Msg::ColPid => "pid",

        Msg::AccountsHeading => "账户",
        Msg::AccountsAvailability => "可用时间",
        Msg::AccountsEmpty => {
            "尚未读取到用量。在窗格中登录 claude 或 codex 后，几分钟内会显示在这里。"
        }
        Msg::TimelineNow => "现在",
        Msg::StatusOk => "正常",
        Msg::StatusNear => "将满",
        Msg::StatusLimit => "已满",
        Msg::StatusNoKey => "未登录",

        Msg::PaneEmpty => "点按以启动 Shell",

        Msg::BannerRestartsPaused => "Marspot 的核心进程反复停止，将自动重试",
        Msg::BannerRecovering => "Marspot 正在恢复…",
        Msg::BannerCrashLoop => "Marspot 多次崩溃，已进入安全模式（保留会话，不启动新进程）",

        Msg::BadgeNoAccountFree => "⚠ 没有可用账户",
        Msg::BadgeNoUsageData => "⚠ 没有用量数据",
        Msg::BadgeModelUnknown => "⚠ 模型未知",
        Msg::BadgeHeldByBgJob => "⚠ 后台任务占用中",

        Msg::Language => "语言",
        Msg::CostLanguage => "立即应用于 Marspot 的面板和菜单。",
        Msg::CostLanguageRelaunch => "系统提供的菜单项在重新启动 Marspot 后切换。",
        Msg::LangEnglish => "English",
        Msg::LangJapanese => "日本語",
        Msg::LangChinese => "简体中文",
    }
}

fn write(a: Arg<'_>, out: &mut dyn fmt::Write) -> fmt::Result {
    match a {
        Arg::MoveToWindow(n) => write!(out, "移到窗口 {n}"),
        Arg::PaneCount { cols, rows } => write!(out, "{cols} × {rows} = {} 个窗格", cols * rows),
        Arg::ProcessDetail { pane, sid, shell_pid } => {
            write!(out, "{pane} · sid {sid} · Shell pid {shell_pid}")
        }
        Arg::ResetAt(at) => write!(out, "重置 {at}"),
        Arg::UpdatedAt(at) => write!(out, "更新于 {at}"),
        Arg::SessionStarting(id) => write!(out, "会话 {id} 正在启动…"),
        Arg::SessionUnavailable(id) => write!(out, "会话 {id} 不可用，按任意键重试"),
        Arg::ProfileRow { n, current: true } => write!(out, "● 配置 {n}"),
        Arg::ProfileRow { n, current: false } => write!(out, "   配置 {n}"),
        Arg::HandOffTo(target) => write!(out, "交接给 {target}"),
        Arg::SwitchToProfile(n) => write!(out, "切换到 P{n}"),
    }
}
