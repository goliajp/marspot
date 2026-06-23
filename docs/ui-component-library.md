# marspot UI — Component Library v4 + Token 补全 spec

> 2026-06-23.写在 [[ui-system-model.md]] v3 框架之上.v3 doc 是
> framework spec(View/Modifier/Constraints/...);本 doc 是
> **构筑在框架上的 component library 设计** + 必要的 **token
> 补全** + **terminal 社区 theme 标准对齐**.
>
> 触发原因:user 报"我们要观察现在在用的和以后很可能会要用的 UI,
> 构建自己的 components,model 不够要补,theme token 应该是要补的
> 东西"+ "社区 terminal theme 插件应该标准化支持,学习他们的
> token 化来补全完善体系".

---

## 目录

- §0 设计目标
- §1 现状 audit — marspot 已有 UI 组件清单
- §2 Terminal 社区 theme 标准对齐(iTerm2 / Base16 / Alacritty /
  Warp / Kitty / Windows Terminal)
- §3 Token 补全 spec(v4)— 完整 token 列表 + 每条来源
- §4 Component Library v4 — 4 级 + 30+ component
- §5 Theme file format(.marspot-theme.toml)
- §6 Migration plan(legacy ViewPainter component → v4)
- §7 Out of scope

---

## 0. 设计目标

1. **完整 token 化** — 每个视觉决定(色/距/线宽/字号/曲率/阴影
   /持续时间)都映射到 named token,**绝不让 component code 写
   字面量**.参照 Base16 / Material Design / SwiftUI semantic
   colors.

2. **Terminal 社区主题兼容** — 接受社区 .itermcolors / .alacritty
   / base16 yaml / Warp json / Windows Terminal json 配置文件,
   parse 进 marspot 的 token 表.

3. **Component library 完整** — chrome 类 UI 该用的 30+ component
   全部 spec 化,有 marspot 现已用的也有未来会用的(SettingsDialog
   / CommandPalette / Toast 等).

4. **基于 v3 framework** — 不发明新框架.每个 component 都是
   modifier chain 组合 + (可选)stateful HostState.legacy
   ViewPainter 路径走 P3i migration 退役.

5. **可被 user / theme designer 扩展** — 提供 theme file format
   而不是写死 const.

---

## 1. 现状 audit — marspot 已有 UI 组件清单

(2026-06-23 通过 audit `src/ui/components/` + `src/ui/system/macos/`
+ `src/render_metal.rs` 整理.)

### 1.1 Component 文件(`src/ui/components/`)

| File | 渲染什么 | 公开 API | 渲染路径 |
|---|---|---|---|
| `button.rs` | 可点击 rect + 可选 label+icon;4 layout mode × 4 style(default/ghost/chrome/destructive) | `Button` struct + `hit_test()` + `paint(&mut ViewPainter)` | ViewPainter |
| `context_menu.rs` | 右键菜单几何;rect + hit_test | `ContextMenu::layout()` → frame + row rects;`hit_test()` → ContextMenuHit | ViewPainter |
| `grid.rs` | row-major cell rects + 可选 seam | `Grid` + `paint()` 委托 `GridSeams` | ViewPainter |
| `grid_item.rs` | 单 grid cell focus outline(8 rects pixel-perfect) | `GridItem { focused, paint }` | ViewPainter |
| `grid_seams.rs` | inter-cell hairline(BG pipeline pixel-perfect) | `GridSeams::paint()` | ViewPainter |
| `layout_modal.rs` | "选 N×M grid shape" 居中 modal + preview card grid + drag hint | `LayoutModal::layout()` + `hit_test()` + 拖拽 helpers | ViewPainter |
| `list_view.rs` | 垂直可选行 + focused highlight;viewport 外截断(无 scroll) | `ListView::row_rect(i)` + `paint()` | ViewPainter |
| `modal_frame.rs` | 几何 chrome:title bar + 可选 tab strip + body + 状态(maximized/minimized/drag offset) | `ModalFrame::layout()` 返子 rects;无 paint | 纯数据 |
| `panel.rs` | View wrapper + uniform padding | `Panel::new() / content_rect() / paint(closure)` | ViewPainter |
| `scroll_view.rs` | viewport rect + offset + content_h;wheel / clamp / visible_row_range | 纯数据,无 paint | 纯数据 |
| `search_overlay.rs` | Panel + TextInput + ListView + Button × 多 + 计数 | `paint_search_overlay(SearchOverlayParams)` | ViewPainter(组合)|
| `sidebar.rs` | 1 entry/session + focus BG + status dot + label | `Sidebar::row_rect(i) / paint()` | ViewPainter |
| `tab_strip.rs` | 等宽 tab + ellipsis truncation | `TabStrip::layout()` + `hit_test()` | 纯几何 |
| `table.rs` | Px/Flex 列 + tree 缩进 + sort indicator + master/detail split | `Table::hit_test_row/header / paint()` | ViewPainter |
| `text_input.rs` | 单行 text field + accent caret | `TextInput::paint()` | ViewPainter |

### 1.2 System chrome(`src/ui/system/macos/`)

| File | 渲染 | API |
|---|---|---|
| `title_bar.rs` | macOS 风 title bar(BG + traffic lights + 居中 title)| `TitleBar::layout / paint / hit_test → TitleBarHit` |
| `traffic_lights.rs` | 3 个 SDF 圆(红/黄/绿)| `TrafficLights::layout / paint(UiRectInstance) / hit_test` |
| `icons/dev_panel.rs` | "panels-right" icon | `DevPanelIcon: IconComponent` |
| `icons/grid.rs` | "layout-grid" icon w/ cells | `GridIcon: IconComponent` |
| `icons/list_tree.rs` | 三横线缩进 | `ListTreeIcon: IconComponent` |
| `icons/sidebar.rs` | "panel-left" outline | `SidebarIcon: IconComponent` |

### 1.3 Inline-painted UI(还没拆 component 的)

- **ProcessPanel** — `src/render_metal.rs:2654+` `push_process_panel_via_view` + `paint_process_panel_content`.frame 走 View,内容 master/detail Table + [×] kill button 手写 ViewPainter 装配.
- **DevPanel 主框架** — tab strip + menu + section 路由 由
  `src/ui/components/dev_panel.rs` 自己手画(不是用 framework
  `tab_strip()` / `list_row()` 接).只 Model section 内部走新 framework.
- **PaneFrame / 分屏 border** — 渲染 path 走 `render_metal.rs` 手画 hairline,无 component 抽象.
- **StatusBar / "marspot is checking for updates" 类 toast** — 当前没有.

### 1.4 Audit 结论

- 15 个 `components/*.rs` + 6 个 `system/macos/*.rs` 全部走 **ViewPainter** 老路径.
- 几个**纯数据** (ModalFrame / ScrollView / TabStrip / ContextMenu layout) 形态正好可以 lift 到 v3 framework 用 modifier 链复刻而不丢逻辑.
- ProcessPanel / DevPanel main / PaneFrame 是 **inline-painted**,完全没 component 抽象,migration 时直接造新 v4 component 就行.

---

## 2. Terminal 社区 theme 标准对齐

社区主流 terminal 都有 token 化的 theme system.我们要**学他们的 token shape,**然后兼容 import,让 marspot 用户能用社区 theme.

### 2.1 主流 theme schema

| 项目 | 文件格式 | 核心 token | 备注 |
|---|---|---|---|
| **iTerm2** | `.itermcolors`(plist XML) | ANSI 16(black/red/green/yellow/blue/magenta/cyan/white × normal/bright)+ Bg/Fg + Cursor + SelectionBg/SelectionFg + BoldColor + LinkColor | 最普及,~10000 community schemes |
| **Base16** | YAML | 8 backgrounds(base00..base07)+ 8 accents(base08..base0F)| 不局限 terminal — 同时给 IDE / status bar / 整 UI 系统提供 base.500+ schemes |
| **Alacritty** | YAML(`alacritty.yml` 一节) | colors.primary(bg/fg)+ colors.normal/bright/dim × 8 ANSI + colors.cursor.{cursor,text} + colors.selection.{background,text} + colors.search.matches/focused_match + colors.hints.start/end | YAML 比 plist 干净 |
| **Kitty** | conf 文本 | `foreground` `background` `cursor` `cursor_text_color` `selection_foreground` `selection_background` + `color0..15`(ANSI)+ `mark1_*` `mark2_*` `mark3_*` + URL underline + Tab Bar | 命名跟 ANSI X11 colors 一致 |
| **Warp** | YAML | terminal_colors(ANSI 16) + accent + 背景 gradient + foreground + cursor | 更多 UI tokens(active_tab/inactive_tab/sidebar/...) |
| **Windows Terminal** | JSON | name + background + foreground + cursorColor + selectionBackground + black..brightWhite | settings.json 数组里 |
| **Hyper** | JS | foregroundColor + backgroundColor + cursorColor + cursorAccentColor + colors{black,red,...,brightWhite} | 主题作 npm 包 |
| **VSCode** Workbench theme | JSON | 500+ tokens(activityBar.background / panel.border / terminal.ansiBlack / editor.foreground / ...)| 真完整 UI tokenization,terminal 是其中一节 |

### 2.2 共识 token list(MUST 实现)

跨 5+ 个上述项目都共同的 token:

**Terminal palette**:
- `terminal.bg`,`terminal.fg`,`terminal.cursor`,`terminal.cursor_text`(光标在字符上时该字的颜色)
- `terminal.selection_bg`,`terminal.selection_fg`
- `terminal.ansi_{black,red,green,yellow,blue,magenta,cyan,white}` × 2(normal + bright)= 16
- `terminal.link`(超链接下划线 + 颜色)
- `terminal.bold_fg`(可选,加粗时的颜色变种)
- `terminal.search.match`,`terminal.search.match_current`

**UI chrome**:
- `chrome.bg`,`chrome.fg`,`chrome.border`
- `chrome.tab.active_bg`,`chrome.tab.active_fg`,`chrome.tab.inactive_bg`,`chrome.tab.inactive_fg`
- `chrome.sidebar.bg`,`chrome.sidebar.fg`,`chrome.sidebar.active_bg`
- `chrome.status_bar.bg`,`chrome.status_bar.fg`
- `chrome.popover.bg`,`chrome.popover.fg`,`chrome.popover.border`

---

## 3. Token v4 — 现状 vs 应有

### 3.1 现状(v3,2026-06-23)

```rust
pub mod color   { FG / FG_MUTED / FG_DISABLED / BG / BG_RAISED / BG_PANEL
                  BG_SELECTED / BG_HOVER / BORDER / DIVIDER / HAIRLINE
                  ACCENT / ACCENT_DIM / SUCCESS / WARN / DANGER
                  SHADOW / HINT }
pub mod space   { XS / SM / MD / LG / XL / XXL }
pub mod radius  { NONE / SM / MD / LG / PILL }
pub mod elev    { E0 / E1 / E2 / E3 }
pub mod text    { CAPTION / BODY / HEADER / LARGE_HEADER / HINT / CODE }
```

加上各 themed::light::* / hc::*.

### 3.2 v4 应加(完整 spec)

**A. Terminal-palette 命名空间 — 全新**

```rust
pub mod terminal {
    use super::Color;

    // ─── Core ─────────────────────────────────────
    pub const BG: Color = ...;
    pub const FG: Color = ...;
    pub const CURSOR_BG: Color = ...;        // 光标块 BG
    pub const CURSOR_FG: Color = ...;        // 光标块上的字色
    pub const SELECTION_BG: Color = ...;
    pub const SELECTION_FG: Color = ...;
    pub const LINK: Color = ...;              // URL / 超链接
    pub const BOLD_FG: Option<Color> = None;  // 加粗用色,None = 同 FG

    // ─── ANSI 16-color ────────────────────────────
    pub mod ansi {
        use super::Color;
        pub const BLACK:   Color = ...;
        pub const RED:     Color = ...;
        pub const GREEN:   Color = ...;
        pub const YELLOW:  Color = ...;
        pub const BLUE:    Color = ...;
        pub const MAGENTA: Color = ...;
        pub const CYAN:    Color = ...;
        pub const WHITE:   Color = ...;
        pub mod bright {
            use super::Color;
            pub const BLACK:   Color = ...;
            // ... 同上 8
        }
    }

    // ─── Search ────────────────────────────────────
    pub mod search {
        pub const MATCH:         Color = ...;
        pub const MATCH_CURRENT: Color = ...;
    }
}
```

**B. UI Chrome 命名空间扩**

```rust
pub mod color {
    // 已有 FG/BG/... 保留 backward compat

    pub const FG_INVERSE: Color = ...;       // BG_SELECTED 上的字色
    pub const FG_LINK: Color = ...;           // 链接 / URL

    // ─── Surface levels(更多分级)──────────────
    pub const SURFACE_0: Color = ...;         // 最低 — 主画布
    pub const SURFACE_1: Color = ...;         // = BG_RAISED
    pub const SURFACE_2: Color = ...;         // = BG_PANEL
    pub const SURFACE_3: Color = ...;         // popover / modal
    pub const SURFACE_4: Color = ...;         // tooltip
    pub const OVERLAY:   Color = ...;         // 半透明遮罩

    // ─── Tab strip ────────────────────────────────
    pub const TAB_ACTIVE_BG: Color = ...;
    pub const TAB_ACTIVE_FG: Color = ...;
    pub const TAB_INACTIVE_BG: Color = ...;
    pub const TAB_INACTIVE_FG: Color = ...;
    pub const TAB_HOVER_BG: Color = ...;

    // ─── Sidebar ──────────────────────────────────
    pub const SIDEBAR_BG: Color = ...;
    pub const SIDEBAR_FG: Color = ...;
    pub const SIDEBAR_ACTIVE_BG: Color = ...;
    pub const SIDEBAR_ACTIVE_FG: Color = ...;

    // ─── Status bar ──────────────────────────────
    pub const STATUS_BAR_BG: Color = ...;
    pub const STATUS_BAR_FG: Color = ...;

    // ─── Diff colors ─────────────────────────────
    pub const DIFF_ADD_BG: Color = ...;
    pub const DIFF_ADD_FG: Color = ...;
    pub const DIFF_REMOVE_BG: Color = ...;
    pub const DIFF_REMOVE_FG: Color = ...;
    pub const DIFF_CHANGE_BG: Color = ...;

    // ─── Severity(4 levels)─────────────────────
    pub const INFO: Color = ...;       // = ACCENT
    pub const SUCCESS: Color = ...;    // 已有
    pub const WARN: Color = ...;       // 已有
    pub const DANGER: Color = ...;     // 已有(=ERROR)
    pub const CRITICAL: Color = ...;    // 红更深,系统级

    // ─── Focus ring(独立于 accent)────────────
    pub const FOCUS_RING: Color = ...;

    // ─── Disabled ────────────────────────────────
    pub const DISABLED_BG: Color = ...;
    pub const DISABLED_FG: Color = ...;
}
```

**C. Motion / animation tokens — 全新**

```rust
pub mod motion {
    pub const INSTANT: f64 = 0.0;          // ms
    pub const FAST: f64 = 120.0;
    pub const NORMAL: f64 = 200.0;
    pub const SLOW: f64 = 350.0;
    pub const VERY_SLOW: f64 = 700.0;

    // Curve presets
    pub use super::AnimCurve::*;
}
```

**D. Border style — 全新**

```rust
pub mod border {
    use super::Length;
    pub const NONE: Length = Length::Pt(0.0);
    pub const HAIRLINE: Length = Length::Pt(0.5);
    pub const THIN: Length = Length::Pt(1.0);
    pub const MEDIUM: Length = Length::Pt(2.0);
    pub const THICK: Length = Length::Pt(3.0);
}
```

**E. Z-index / layer tokens — 全新**

```rust
pub mod layer {
    pub const CONTENT: i32 = 0;
    pub const STATUS: i32 = 10;
    pub const STICKY: i32 = 100;
    pub const TOOLBAR: i32 = 200;
    pub const POPOVER: i32 = 1000;
    pub const MODAL: i32 = 2000;
    pub const TOAST: i32 = 3000;
    pub const TOOLTIP: i32 = 4000;
    pub const SYSTEM: i32 = 9999;
}
```

### 3.3 Token v4 落地策略

- v3 existing const 保留,不破 backward compat
- v4 全部加在 `theme::token` namespace 下
- 每个 v4 const 也走 Dark / Light / HC 三套
- `themed::*` 闭包查 active palette
- Component code 全部走 `theme::*` 而非字面量(已是规则)

---

## 4. Component Library v4 — 完整 spec

按层级组织.每个 component 标注:**[v3 framework ✓]**(framework
原子已够)/ **[v4 加]**(新 component)/ **[migrate]**(legacy →
v3).

### 4.1 Level 0 — Atoms(framework 已有)

`Text` / `Spacer` / `Filled` / `Hairline` / `Image` / `Shape` /
`Canvas`

→ ✅ 全部 land,无 action.

### 4.2 Level 1 — Containers(framework 已有)

`VStack` / `HStack` / `ZStack` / `ScrollView` / `LazyVStack` /
`LazyHStack` / `Grid` / `VariableGrid`

→ ✅ 全部 land.

### 4.3 Level 2 — Stateful primitives(framework 已有)

`Toggle` / `Picker`

→ 已有.
**[v4 加]**:`TextField`(P3i+),`Slider`,`Stepper`,`Checkbox`,
`RadioButton`,`ColorPicker`.

### 4.4 Level 3 — Composable presets(framework v3 已有 5 个 +)

| Preset | 现状 | 设计 |
|---|---|---|
| `card(child)` | ✅ | bg_raised + border + radius + shadow E1 |
| `panel(child)` | ✅ | bg_panel + padding + radius |
| `badge(label, c)` | ✅ | pill |
| `tooltip(label)` | ✅ | popup + border + shadow E2 |
| `tab_strip(labels, sel, ActionId)` | ✅ | horizontal tabs |
| `context_menu(items, divider, ActionId)` | ✅ | card + rows |
| `breadcrumb(segments)` | ✅ | Home › ... › |
| `list_row(label, trailing, selected, ActionId)` | ✅ | sidebar/table 行 |
| **`avatar(initial, c)`** | **[v4 加]** | 圆形 initials + bg color |
| **`tag(label, c)`** | **[v4 加]** | 类 badge 但 outline 风 |
| **`chip(label, leading_icon, on_close)`** | **[v4 加]** | tag + 可关闭 ✕ |
| **`empty_state(icon, title, hint)`** | **[v4 加]** | "no items" 占位 |
| **`skeleton(width, height)`** | **[v4 加]** | 加载骨架占位 |
| **`spinner(size)`** | **[v4 加]** | 旋转 indicator(配合 Anim) |
| **`progress_bar(value)`** | **[v4 加]** | 0..1 横向进度 |
| **`progress_ring(value)`** | **[v4 加]** | 0..1 圆环进度(SDF arc 留 v2+) |
| **`kbd(shortcut)`** | **[v4 加]** | `⌘K` 键帽 chip |
| **`divider()`** | **[v4 加]** | 语义化 divider(不是只 hairline) |
| **`banner(severity, message, on_dismiss)`** | **[v4 加]** | 顶部条状 alert |

### 4.5 Level 4 — Overlay(stateful patterns)

| Component | 现状 | 设计 |
|---|---|---|
| `ContextMenu` | preset ✅ | + 真 hover-track + 子菜单 |
| `Tooltip` | preset ✅ | + 真 hover-trigger + 边界自动 reposition |
| **`Modal(content, on_dismiss)`** | **[v4 加]** | full-window overlay + centered card + dismiss |
| **`Sheet(content, side)`** | **[v4 加]** | 侧边滑入(类 macOS sheet) |
| **`Popover(anchor, content)`** | **[v4 加]** | 锚定 anchor + 箭头 + 自动 reposition |
| **`Toast(severity, message)`** | **[v4 加]** | 短时 notification + auto dismiss |
| **`ConfirmDialog(title, body, confirm, cancel)`** | **[v4 加]** | Modal preset:确认对话框 |
| **`AlertDialog(severity, title, body, OK)`** | **[v4 加]** | Modal preset:警告对话框 |

### 4.6 Level 5 — Data display

| Component | 现状 | 设计 |
|---|---|---|
| `list_row` preset | ✅ | atomic 行 |
| **`List(items, render_row, sel_id)`** | **[v4 加]** | 完整 list view + multi-select + 键盘导航 |
| **`Tree(nodes, expand_id, ...)`** | **[v4 加]** | 折叠树 — sidebar / file picker |
| **`Table(columns, rows, sort)`** | legacy `Table` | **[migrate]** 迁到 v3 framework |
| **`KeyValueList(entries)`** | **[v4 加]** | Settings 风格 — left label / right value |
| **`CodeBlock(code, lang)`** | **[v4 加]** | mono 字体 + syntax 着色 + 复制按钮 |
| **`Diff(before, after, mode)`** | **[v4 加]** | side-by-side / inline diff |
| **`Timeline(events)`** | **[v4 加]** | event log w/ icons + 时间戳 |

### 4.7 Level 6 — Navigation

| Component | 现状 | 设计 |
|---|---|---|
| `tab_strip` | ✅ preset | + 真 scrollable overflow |
| `breadcrumb` | ✅ preset | + 真 click navigation |
| **`SideNav(sections, items, sel)`** | **[v4 加]** | 多 section sidebar |
| **`CommandPalette(items, query, on_pick)`** | **[v4 加]** | ⌘P / ⌘⇧P 风格 picker |
| **`BackButton(label, on_back)`** | **[v4 加]** | 系统风 back chevron |

### 4.8 Level 7 — Buttons(完整 spec)

| Component | 现状 | 设计 |
|---|---|---|
| `Button` | legacy ViewPainter | **[migrate]** v3:`button(label, style, ActionId)` |
| **`IconButton(icon, ActionId)`** | **[v4 加]** | icon-only,sized for chrome |
| **`MenuButton(label, items, ActionId)`** | **[v4 加]** | dropdown |
| **`ButtonGroup(buttons)`** | **[v4 加]** | 横向连续 button(单 border)|
| **`SplitButton(label, menu_items)`** | **[v4 加]** | 主 action + dropdown 二段 |
| **`SegmentedControl(options, sel)`** | = Picker | 同 `Picker`,语义别名 |

### 4.9 Level 8 — Marspot-specific chrome

| Component | 现状 | 设计 |
|---|---|---|
| `TitleBar` | legacy ✅ | **[migrate]** v3:traffic_lights + tab_strip + StatusIcons |
| `TrafficLights` | legacy ✅ | **[migrate]** v3:`shape_circle` × 3 + hit_test |
| `TabStrip`(marspot tab bar) | legacy ✅ | **[migrate]** 走 v3 preset + 内部 scroll |
| `Sidebar`(pane list) | legacy ✅ | **[migrate]** 走 v3 `lazy_vstack` + `list_row` |
| `Table`(ProcessPanel master/detail) | legacy ✅ | **[migrate]** 走 v3 `Table` |
| `ContextMenu` | legacy + preset | **[migrate]** 真组件用 preset 替换 |
| `LayoutModal` | legacy ✅ | **[migrate]** 走 v3 `Modal` + `grid` + drag |
| `SearchOverlay` | legacy ✅ | **[migrate]** 走 v3 `Popover` + TextField + `List` |
| `ProcessPanel` | 手画(render_metal.rs) | **[v4 + migrate]** 走 v3 `Modal` + `Table` + IconButton |
| `DevPanel main` | 半 legacy 半新 | **[migrate]** 整体走 v3 framework(`tab_strip` + sidebar + ScrollView) |
| **`StatusBar`** | 不存在 | **[v4 加]** 底部 status + 右下角 chip |
| **`PaneFrame`** | render_metal.rs 手画 | **[v4 + migrate]** v3 component,split border + drag handle |
| **`UpdateBanner`** | 不存在 | **[v4 加]** 顶部 banner:"new version 0.7.0 available" |
| **`CrashOverlay`** | 不存在 | **[v4 加]** session crash:retry / detach / view log |

---

## 5. Theme file format(`.marspot-theme.toml`)

让 user / theme designer 写文件直接换 token,community 兼容 import.

### 5.1 Format spec

```toml
[meta]
name = "Tokyonight Storm"
author = "enkia"
license = "MIT"
description = "..."
based_on = "tokyonight"   # 引用 base16 / itermcolors / ...

[terminal]
bg = "#1a1b26"
fg = "#a9b1d6"
cursor_bg = "#c0caf5"
cursor_fg = "#1a1b26"
selection_bg = "#33467c"
selection_fg = "#a9b1d6"
link = "#9ece6a"
bold_fg = "#ffffff"

[terminal.ansi]
black   = "#15161e"
red     = "#f7768e"
green   = "#9ece6a"
yellow  = "#e0af68"
blue    = "#7aa2f7"
magenta = "#bb9af7"
cyan    = "#7dcfff"
white   = "#a9b1d6"

[terminal.ansi.bright]
black   = "#414868"
red     = "#f7768e"
# ... 8 colors

[terminal.search]
match = "#3d59a1"
match_current = "#7aa2f7"

[chrome.color]
fg = "#a9b1d6"
fg_muted = "#787c99"
bg = "#1a1b26"
bg_raised = "#1f2335"
bg_panel = "#24283b"
bg_selected = "#33467c"
border = "#414868"
accent = "#7aa2f7"
accent_dim = "#bb9af7"
success = "#9ece6a"
warn = "#e0af68"
danger = "#f7768e"

[chrome.color.tab]
active_bg = "#24283b"
active_fg = "#c0caf5"
inactive_bg = "#1a1b26"
inactive_fg = "#787c99"

# ... 同上结构对 sidebar / status_bar / popover / surface_*
```

### 5.2 Import adapters

- `marspot theme import --from itermcolors <file>` — parse `.itermcolors` plist 映射到 marspot tokens
- `marspot theme import --from base16 <file>` — base16 yaml
- `marspot theme import --from alacritty <file>` — alacritty yml
- `marspot theme import --from kitty <file>` — kitty conf
- `marspot theme import --from warp <file>` — warp yaml
- `marspot theme import --from windows-terminal <file>` — wt json

Adapter 在 `MARSPOT_STATE_DIR/themes/` 写一个 `.marspot-theme.toml`,用户用 `theme::load_from_file()` 切换.

### 5.3 Built-in themes

ship 一批已 import:

- `dark`(默认,marspot 调色)
- `light`(marspot 调色)
- `high_contrast`(marspot 调色)
- `tokyonight` — base16
- `gruvbox_dark` — base16
- `nord` — base16
- `solarized_dark` / `solarized_light`
- `monokai`
- `dracula`
- `catppuccin_macchiato`

---

## 6. Migration plan(legacy → v4)

按依赖排序:

```
[P0]  补 Token v4 — color/motion/border/layer/terminal palette/text/elev
[P0]  themed::* 完整化(所有 v4 token 走 themed::color::* 闭包)
[P0]  ThemeFile load_from_toml + 5 import adapter

[P1]  L3 preset 加:avatar/tag/chip/empty_state/skeleton/spinner/
      progress_bar/progress_ring/kbd/divider/banner
[P1]  L4 Overlay:Modal/Sheet/Popover/Toast/ConfirmDialog/AlertDialog
[P1]  L5 Data:List/Tree/KeyValueList/CodeBlock/Diff/Timeline
[P1]  L6 Nav:CommandPalette/SideNav/BackButton
[P1]  L7 Button family

[P2 = Phase B P3i,在 legacy 之上 land v4]
[P2.1] DevPanel main framework → tab_strip + side_nav + ScrollView
[P2.2] ContextMenu legacy → preset 替换 + 真 hover
[P2.3] Sidebar legacy → lazy_vstack + list_row
[P2.4] Table legacy → v4 Table
[P2.5] LayoutModal → v4 Modal + grid
[P2.6] SearchOverlay → v4 Popover + TextField + List
[P2.7] ProcessPanel → v4 Modal + v4 Table + IconButton
[P2.8] TitleBar / TrafficLights → v3 chrome
[P2.9] PaneFrame → v4 component
[P2.10] StatusBar / UpdateBanner / CrashOverlay 新加

[P3]  ViewPainter 真退役(legacy 文件全部删完)
```

---

## 7. Out of scope(明确不做)

- **完整 CSS spec** — token spec 拿心智模型,不实现 cascading / specificity
- **Theme 互动编辑器** — v4 只 file-based theme;GUI theme editor 留 v2+
- **真 NSAccessibility 接口** — Modifier API land,真 NSAccessibility 桥 留 v2+
- **真 TextField NSTextInputClient** — 类型签名 land,IME 集成 留 v2+
- **每 component 的真 animation 演示** — 数据层 land,真 schedule 整合靠 [A3]
- **跨平台 chrome**(Linux / Windows) — marspot macOS-only

---

## 一句话

**marspot UI v4 = v3 framework(View/Modifier/Constraints/Anim/Lifecycle)+ Token v4(增 terminal palette + tab/sidebar/status/diff/severity4/focus/motion/border/layer)+ Component Library 8 levels(0 Atoms / 1 Containers / 2 Stateful / 3 Composable presets [20+] / 4 Overlay / 5 Data display / 6 Navigation / 7 Button family / 8 Marspot-specific chrome)+ ThemeFile TOML format(5 import adapter for community themes)+ 11 built-in themes.**

整套 spec 跟 macOS / VSCode / Material Design / Base16 同水准,
**不是发明新轮子,是把 marspot 的 UI 体系 align 到 industry
standard**.
