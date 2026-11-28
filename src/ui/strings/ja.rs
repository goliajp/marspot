//! Japanese. Standard commands are worded as macOS words them; labels
//! are nouns or plain verbs and take no full stop, descriptions are
//! sentences and do.

use std::fmt;

use super::{Arg, Lang, Msg};

pub(super) static LANG: Lang = Lang { text, write };

fn text(m: Msg) -> &'static str {
    match m {
        Msg::SettingsIdleReclamation => "未使用ペインの休止",
        Msg::SettingsAppearance => "外観",
        Msg::SettingsClaudeCode => "Claude Code",
        Msg::SettingsCodex => "Codex",
        Msg::SettingsScrolling => "スクロール",

        Msg::ReclaimIdlePanes => "未使用のClaudeペインを休止",
        Msg::IdleFor => "休止までの時間",
        Msg::WarmUpOnReturn => "戻ったときに順次再開",
        Msg::Theme => "テーマ",
        Msg::DimOtherPanes => "操作中でないペインを暗くする",
        Msg::HelperStrip => "ヘルパーをペインの下に一覧表示",
        Msg::CircledDigitsWide => "丸数字を2セル幅で表示",
        Msg::ClaudeCodeReportsModel => "Claude Codeから使用モデルを取得",
        Msg::ClaudeCodeGuardModel => "全アカウント共通のデフォルトモデルを保護",
        Msg::ClaudeCodeSchedProfile => "アカウント間のペイン切り替え",
        Msg::CodexSchedCredits => "Codexの有料credits",
        Msg::WheelSpeed => "ホイールの速度",

        Msg::CostReclaimIdlePanes => "ペインに戻ると、セッションの再開に約3秒かかります",
        Msg::CostIdleFor => "ターミナルの無操作時間ではなく、セッション記録の最終更新時刻で判定します",
        Msg::CostWarmUpOnReturn => "ペインに戻ったとき、休止中のペインを1秒に1つずつ再開します",
        Msg::CostTheme => "以前の色を取得したプログラムは、再取得するまでその色のままです",
        Msg::CostDimOtherPanes => "暗くするほど見分けやすくなりますが、暗すぎると読みにくくなります",
        Msg::CostHelperStrip => "ヘルパーがいる間は、その行数だけペインの表示が短くなります",
        Msg::CostCircledDigitsWide => "全角と同じ幅になりますが、折り返しが1桁早まることがあります",
        Msg::CostClaudeCodeReportsModel => {
            "Claude Codeの設定にステータスラインを追加します。既存の設定は引き継がれ、オフにすると元に戻ります"
        }
        Msg::CostClaudeCodeGuardModel => "/modelは送信前に確認します。確認されていない変更は元に戻します",
        Msg::CostCodexSchedCredits => "「最後に使う」では週の枠を使い切ったアカウントのペインをターンの合間にプラン枠の残るアカウントへ移します。他の2つは今は移しません",
        Msg::CostClaudeCodeSchedProfile => "確認しながら段階的に有効化。積極的なほどFable枠の失効が減り、切り替えで会話を読み直します",
        Msg::CostWheelSpeed => "速いほど少ない操作で届きますが、行き過ぎやすくなります",

        Msg::MenuCopy => "コピー",
        Msg::MenuPaste => "ペースト",
        Msg::MenuClearScrollback => "スクロールバックを消去",
        Msg::MenuClosePane => "ペインを閉じる",

        Msg::CmdAbout => "Marspotについて",
        Msg::CmdSettings => "設定…",
        Msg::CmdHide => "Marspotを非表示",
        Msg::CmdHideOthers => "ほかを非表示",
        Msg::CmdShowAll => "すべてを表示",
        Msg::CmdQuit => "Marspotを終了",
        Msg::CmdNewWindow => "新規ウインドウ",
        Msg::CmdNewPane => "新規ペイン",
        Msg::CmdClosePanel => "パネルを閉じる",
        Msg::CmdFind => "検索",
        Msg::CmdPrevPrompt => "前のコマンドへ移動",
        Msg::CmdNextPrompt => "次のコマンドへ移動",
        Msg::CmdToggleSidebar => "サイドバーを表示/非表示",
        Msg::CmdLayout => "レイアウト",
        Msg::CmdProcesses => "プロセスモニタ",
        Msg::CmdAccounts => "エージェントアカウント",
        Msg::CmdFullScreen => "フルスクリーンにする/解除",
        Msg::CmdMinimize => "しまう",
        Msg::CmdPrevPane => "前のペイン",
        Msg::CmdNextPane => "次のペイン",
        Msg::CmdPane1 => "ペイン1",
        Msg::CmdPane2 => "ペイン2",
        Msg::CmdPane3 => "ペイン3",
        Msg::CmdPane4 => "ペイン4",
        Msg::CmdPane5 => "ペイン5",
        Msg::CmdPane6 => "ペイン6",
        Msg::CmdPane7 => "ペイン7",
        Msg::CmdPane8 => "ペイン8",
        Msg::CmdPane9 => "ペイン9",
        Msg::CmdPaneRange => "ペイン1〜9",
        Msg::CmdShortcuts => "キーボードショートカット",
        Msg::MenuBarApp => "Marspot",
        Msg::MenuBarFile => "ファイル",
        Msg::MenuBarEdit => "編集",
        Msg::MenuBarView => "表示",
        Msg::MenuBarWindow => "ウインドウ",
        Msg::MenuBarHelp => "ヘルプ",
        Msg::PanelShortcuts => "キーボードショートカット",
        Msg::PanelSettings => "設定",
        Msg::PanelProcesses => "プロセスモニタ",
        Msg::PanelAccounts => "エージェントアカウント",

        Msg::Idle15m => "15分",
        Msg::Idle30m => "30分",
        Msg::Idle1h => "1時間",
        Msg::Idle2h => "2時間",
        Msg::IdleNever => "しない",

        Msg::DimOff => "オフ",
        Msg::DimLight => "弱",
        Msg::DimNormal => "標準",
        Msg::DimDeep => "強",

        Msg::ThemeSystem => "システム",
        Msg::ThemeDark => "ダーク",
        Msg::ThemeLight => "ライト",
        Msg::ThemeContrast => "ハイコントラスト",

        Msg::ScrollSlow => "遅い",
        Msg::ScrollNormal => "標準",
        Msg::ScrollFast => "速い",
        Msg::ScrollFaster => "さらに速い",

        Msg::MenuNewPane => "新規ペイン",
        Msg::MenuToggleSidebar => "サイドバーを表示/非表示",
        Msg::MenuOpenLayout => "レイアウトを開く…",
        Msg::MenuMoveToNewWindow => "新しいウインドウに移動",
        Msg::MenuBelongsTo => "所属先…",
        Msg::MenuBelongsToBlocked => "子ペインを持つペインは、ほかのペインに所属できません",
        Msg::MenuMakeTopLevel => "トップレベルのペインにする",
        Msg::MenuReplaceWith => "…と入れ替え",
        Msg::MenuNewShell => "新しいシェル",
        Msg::CloseChildrenAsk => "このペインを閉じると、次の子ペインも終了します：",
        Msg::CloseChildrenConfirm => "すべて閉じる",
        Msg::CloseChildrenCancel => "キャンセル",
        Msg::CloseAskNowhere => "このペインには子ペインがあるため閉じる前に確認しますが、このウィンドウには確認を表示できるペインがありません。ペインを表示しているウィンドウから閉じてください",

        Msg::LinkCopyUuid => "UUIDをコピー",
        Msg::LinkOpenUrl => "URLを開く",
        Msg::LinkCopyUrl => "URLをコピー",
        Msg::LinkOpenFile => "ファイルを開く",
        Msg::LinkRevealFile => "内包しているフォルダを開く",
        Msg::LinkCopyPath => "パスをコピー",
        Msg::LinkSendEmail => "メールを送信",
        Msg::LinkCopyEmail => "メールアドレスをコピー",
        Msg::LinkOpenInBrowser => "ブラウザで開く",
        Msg::LinkCopyIp => "IPアドレスをコピー",

        Msg::LayoutGrid => "グリッド",
        Msg::LayoutArrangement => "配置",
        Msg::LayoutApply => "適用",
        Msg::LayoutColumns => "列",
        Msg::LayoutRows => "行",

        Msg::ProcessPanes => "ペイン",
        Msg::ProcessProcesses => "プロセス",
        Msg::ColPane => "ペイン",
        Msg::ColPids => "プロセス数",
        Msg::ColCpu => "CPU%",
        Msg::ColRss => "RSS",
        Msg::ColProcess => "プロセス",
        Msg::ColPid => "pid",
        Msg::ProcessLeftGroup => "閉じたペインの残り",
        Msg::ProcessLeftDetail => "ペインを閉じた後も動いています。marspotが自分で終了することはありません",

        Msg::PanelHelpers => "ヘルパー",
        Msg::HelpersEmpty => "実行中のヘルパーはありません",
        Msg::HelperViewHere => "ここで表示",
        Msg::HelperViewBack => "このセッションに戻る",
        Msg::HelperEnded => "終了",
        Msg::HelperMoveToWindow => "ウインドウに移動",
        Msg::HelperNotMoved => "ウインドウに移動できませんでした",
        Msg::HelperClose => "閉じる",
        Msg::HelpersCloseAll => "このセッションのヘルパーをすべて閉じる",
        Msg::HelpersDeny => "このディレクトリからのペイン操作を禁止",
        Msg::HelpersAllow => "このディレクトリからのペイン操作を許可",

        Msg::AccountsHeading => "アカウント",
        Msg::AccountsAvailability => "利用可能になる時刻",
        Msg::AccountsEmpty => "使用状況データはまだありません。ペインでClaudeまたはCodexにサインインすると、数分以内に表示されます",
        Msg::TimelineNow => "現在",
        Msg::StatusOk => "利用可",
        Msg::StatusNear => "残りわずか",
        Msg::StatusLimit => "上限到達",
        Msg::StatusNoKey => "未サインイン",

        Msg::ModelGuardNotice => {
            "/modelはこのMacのすべてのClaude Codeアカウントのデフォルトモデルを変更します。\
             送信するにはy、編集に戻るにはEscを押してください"
        }
        Msg::ModelSendRefused => {
            "プログラムがClaude Codeに/modelを送ろうとしたため、送信しませんでした。\
             全アカウントのデフォルトモデルはキーボードからのみ変更できます"
        }
        Msg::ClaudeSettingsChanged => "全アカウント共通のClaude Codeの設定が変更されました",
        Msg::ModelUnset => "既定値",
        Msg::PaneEmpty => "クリックしてシェルを開始",

        Msg::BannerRestartsPaused => "Marspotのコアが繰り返し停止しているため、自動的に再試行します",
        Msg::BannerRecovering => "Marspotを復旧しています…",
        Msg::BannerCrashLoop => {
            "Marspotが繰り返しクラッシュしたため、セーフモードで起動しました（セッションは保持、\
             新規起動は停止）"
        }

        Msg::BadgeNoAccountFree => "⚠ 空きアカウントなし",
        Msg::BadgeNoUsageData => "⚠ 使用状況データなし",
        Msg::BadgeModelUnknown => "⚠ モデル不明",
        Msg::BadgeHeldByBgJob => "⚠ バックグラウンドジョブ実行中",
        Msg::BadgeCodexTurnRunning => "⚠ ターン実行中のため切り替えなし",
        Msg::BadgeCodexAgentRunning => "⚠ サブエージェント実行中のため切り替えなし",
        Msg::BadgeCodexQueued => "⚠ キューにメッセージがあるため切り替えなし",
        Msg::BadgeCodexCustomCommand => "⚠ カスタム引数のため切り替えなし",
        Msg::BadgeCodexStateUnknown => "⚠ セッション状態不明のため切り替えなし",
        Msg::BadgeCodexSwitching => "⚠ 切り替え中",
        Msg::BadgeCodexNotTracked => "⚠ セッション未追跡のため切り替えなし",
        Msg::BadgeCodexThreadUnknown => "⚠ 会話のIDが不明のため切り替えなし（新しい会話も始めません）",
        Msg::BadgeCodexWaitingRelease => "旧アカウントの解放待ち",
        Msg::BadgeCodexStillLoaded => "⚠ 旧アカウントがセッションを保持中のため切り替えなし",

        Msg::Language => "言語",
        Msg::CostLanguage => "Marspotのパネルとメニューにすぐ反映されます",
        Msg::FontFamily => "フォント",
        Msg::CostFontFamily => "すべてのペインが新しい文字幅で折り返し直され、全画面のプログラムは描き直されます",
        Msg::FontSize => "フォントサイズ",
        Msg::CostFontSize => "大きいほど読みやすくなりますが、ペインに収まる行数と桁数が減ります",
        Msg::CostLanguageRelaunch => {
            "システムが提供するメニュー項目は、Marspotの再起動後に切り替わります"
        }
        Msg::LangEnglish => "English",
        Msg::LangJapanese => "日本語",
        Msg::LangChinese => "简体中文",

        Msg::SettingsAgentClis => "エージェントCLI",
        Msg::InstallClaude => "claudeが無ければインストール",
        Msg::InstallCodex => "codexが無ければインストール",
        Msg::InstallGemini => "geminiが無ければインストール",
        Msg::InstallKimi => "kimiが無ければインストール",
        Msg::CostInstallCli => "配布元からダウンロードします。「しない」以外では更新も確認なしで追従します",
        Msg::ConsentAsk => "確認する",
        Msg::ConsentYes => "インストール",
        Msg::ConsentNo => "しない",
        Msg::SchedSteady => "慎重",
        Msg::SchedBalanced => "バランス",
        Msg::SchedFull => "使い切る",
        Msg::CreditsHold => "保留",
        Msg::CreditsLast => "最後に使う",
        Msg::CreditsFree => "制限なし",
    }
}

fn write(a: Arg<'_>, out: &mut dyn fmt::Write) -> fmt::Result {
    match a {
        Arg::MoveToWindow(n) => write!(out, "ウインドウ{n}に移動"),
        Arg::PaneCount { cols, rows } => write!(out, "{cols} × {rows} = {}ペイン", cols * rows),
        Arg::ProcessDetail { pane, sid, shell_pid } => {
            write!(out, "{pane} · sid {sid} · シェルPID {shell_pid}")
        }
        Arg::ResetAt(at) => write!(out, "リセット {at}"),
        Arg::UpdatedAt(at) => write!(out, "更新 {at}"),
        Arg::SessionStarting(id) => write!(out, "セッション{id}を開始しています…"),
        Arg::SessionUnavailable(id) => {
            write!(out, "セッション{id}を利用できません（任意のキーで再試行）")
        }
        Arg::ProfileRow { n, current: true } => write!(out, "● アカウント{n}"),
        Arg::ProfileRow { n, current: false } => write!(out, "   アカウント{n}"),
        Arg::HandOffTo(target) => write!(out, "{target}に引き継ぐ"),
        Arg::SwitchToProfile(n) => write!(out, "P{n}に切り替える"),
        Arg::Helpers(n) => write!(out, "ヘルパー（{n}）"),
        Arg::HelpersOf(n) => write!(out, "ヘルパー（{n}）…"),
        Arg::ChildOf(code) => write!(out, "（#{code:04} の子ペイン）"),
        Arg::MoreChildren(n) => write!(out, "…ほか {n} 件"),
        Arg::WindowFull(n) => write!(out, "このウィンドウにはすでに {n} 個のペインがあり、これ以上置けません"),
        Arg::RanFor { minutes: m @ 0..60 } => write!(out, "{m}分"),
        Arg::RanFor { minutes: m } => write!(out, "{}時間{}分", m / 60, m % 60),
        Arg::ModelPutBack { to, back } => {
            write!(out, "全アカウント共通のClaude Codeのデフォルトモデルが確認なしで{to}に変更されたため、{back}に戻しました")
        }
        Arg::ModelNowIs(to) => write!(out, "全アカウント共通のClaude Codeのデフォルトモデルは{to}になりました"),
        Arg::WebOpening(url) => write!(out, "{url} を開いています…"),
        Arg::WebPageRow(url) => write!(out, "ウェブページ {url}"),
        Arg::WebOpenRow(url) => write!(out, "ウェブページ {url} を開く"),
        Arg::WebPasteCut(mb) => write!(out, "クリップボードの先頭 {mb} MB だけをページに貼り付けました"),
        Arg::WebVersion { side, theirs: Some((a, b)), ours: (c, d) } => {
            write!(out, "madcawl の {side} はバージョン {a}–{b}、marspot は {c}–{d} に対応しています")
        }
        Arg::WebVersion { side, theirs: None, ours: (c, d) } => {
            write!(out, "madcawl の {side} は marspot が対応するバージョン {c}–{d} より古いです")
        }
        Arg::WebFailed(why) => write!(out, "ページを表示できません：{why}"),
        Arg::InstallAsk { cli, place, megabytes } => write!(
            out,
            "{cli}を{place}にインストールしますか？配布元からダウンロードし、約{megabytes} MBを使います。\
             Ctrl-Option-Yでインストール、Ctrl-Option-Nで今後もインストール・更新しません。ほかのキーはペインに送られます"
        ),
    }
}
