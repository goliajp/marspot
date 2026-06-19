# marspot UI Kit — Rules

> 这份文件是 marspot UI 的**唯一权威**.写 UI 之前先读一遍.
> 不允许绕过、不允许在 scene 代码里手写 ui_rect / glyph / cell instance.

## 1. 三层分工(folder = layer)

```
src/ui/
├─ core/           # 基础抽象:View + Painter + Backdrop
├─ system/         # 平台风格 chrome(目前只有 macos/)
└─ components/     # 复用 widget(modal frame / tab strip / scroll view / search overlay / ...)
```

| 层 | 谁能放进来 | 例子 | API 风格 |
|---|---|---|---|
| `core/` | "任何 UI 都需要"的最底层抽象,跟具体外观无关 | `View`、`ViewPainter`、`ViewStyle`、`Backdrop` | 一个核心类型 + 一组配置 + paint() |
| `system/<platform>/` | 平台原生 chrome,带文化共识(用户预期某种外观) | `traffic_lights`(红黄绿三圆点)、`title_bar`(macOS 标题栏) | 一个 widget 一个文件,layout() → paint() → hit_test() |
| `components/` | 跟平台无关、跨场景复用的复合 widget | `modal_frame`、`tab_strip`、`scroll_view`、`search_overlay` | layout() 算 sub-rect,paint() push 实例 |

**判定流程**(从上往下):

1. 是不是 marspot 任何 UI 都要的最低抽象? → `core/`
2. 是不是某平台用户预期的 chrome 形状? → `system/<platform>/`
3. 是不是多个 scene 会用的 widget? → `components/`
4. 都不是,只这一个 scene 用 → **不进 ui kit**,在 scene 文件里 inline(参考 React colocate 原则)

## 2. 铁律(violation = 别 PR)

### 2.1 任何 overlay 必须走 `View`

不允许 scene 直接 push 到 main scratches(`cells_scratch` / `glyphs_scratch` / `ui_rects_scratch`).

> 历史:F1+11 search bar、F3+1.4 process panel,两次"装上去发现还是透"都是
> 同一根:Metal pipeline 顺序固定 BG→DOT→UI→FG,scene 把 BG 走 UI pass 但
> grid FG glyph 在 UI pass 之后画,盖过去就是透.
>
> 一次性根治:`core::View` 在 paint() 时永远 push 到 **overlay scratches**
> (`overlay_cells` / `overlay_glyphs` / `overlay_ui_rects`),encoder 在主 4
> pass 之后多跑 BG→UI→FG 三个 pass 读 overlay scratches.
>
> 只要 scene 用 `View`,这条不可能再坏.

### 2.2 `ViewStyle::default().bg[3] == 1.0`

新做 widget 不允许把 `bg`(或任何前景色)的 alpha 默认设成 < 1.0.

`default_style_is_opaque` 测试钉死这条 invariant.要透明背景必须显式改 alpha
而且通过 PR review 解释 why.

### 2.3 marspot 顶部标题条永远在最上

任何 backdrop 必须用 `Backdrop::Dim { exclude_above_y: top_inset }`.

`exclude_above_y` 是 `Backdrop::Dim` 的**必填字段**(不是 Option),scene
必须显式给一个值,无法忘.

### 2.4 文字 / cell 实例不能从 scene 代码直接 push

scene 不允许写:

```rust
cells.push(CellInstance { ... });           // ✗
glyphs.push(GlyphInstance { ... });         // ✗
ui_rects.push(UiRectInstance { ... });      // ✗
push_text_run(...);                         // ✗(老 free fn,只允许 ui kit 内部用)
```

scene 必须写:

```rust
view.paint(&mut painter, |p| {
    p.fill_rect(rect, color);                       // ✓
    p.fill_rounded_rect(rect, color, r, border);    // ✓
    p.text(x, baseline_y, "hello", color);          // ✓
});
```

`p.text` 内部走 `push_text_run`,但调用者看不到这个细节.

### 2.5 没有 custom paint 闭包

`Button` 的 `IconSpec` 只支持两个变体:

```rust
pub enum IconSpec<'a> {
    Glyph(&'a str),                       // ✓
    Component(&'a dyn IconComponent),     // ✓
    // NO Custom(&'a dyn Fn(...))         — 禁止
}
```

需要新形状的图标:在 `system/<platform>/icons/` 或 `components/icons/`
开一个文件,impl `IconComponent` trait,带 unit test.scene 不允许用闭包
画 icon —— 闭包是漏点,被画过的形状没人能复用 / 检视 / 测.

### 2.6 View 可嵌套(React Native 风)

`View::child(offset, style)` 把 offset(相对父 view 左上)翻译成绝对
坐标,返回新的 child View.嵌套任意层:

```rust
let parent = View::new(absolute_rect, parent_style);
parent.paint(p, |p| {
    let inner = parent.child(
        Rect { x: 10.0, y_top: 10.0, w: 200.0, h: 50.0 },
        ViewStyle::default(),
    );
    inner.paint(p, |p| {
        let deeper = inner.child(Rect { x: 5.0, y_top: 5.0, w: 40.0, h: 30.0 },
            ViewStyle::default());
        deeper.paint(p, |p| { p.text(...) });
    });
});
```

每个 child 都进 overlay scratches,push 顺序 = z 顺序,parent 在底,
child 在上.**没有自动裁剪** —— child 画到父矩形外仍会画出来,把
parent rect 当**设计意图**,不是物理边界.

## 3. 怎么用 — 参考 React

### 3.1 一个 scene 一个文件

参考 React 组件 colocate 习惯.例如 Process Monitor:

- 通用 widget 用 `ui::components::{ModalFrame, TabStrip, ScrollView}`
- 平台 chrome 用 `ui::system::macos::{TrafficLights, TitleBar}`
- modal 弹层基类 `ui::core::View`
- scene 私有逻辑(行内容、kill button 行布局、hit_test 路由)写在
  `marspot-core.rs` 的 `paint_process_panel_content` + `build_process_panel_render`
  本地函数里 —— **跟 scene 的状态机紧贴在一起,不放进 ui kit**

未来 scene 多了(bookmark、profile picker、notification toast)可以开
`src/ui/scenes/`,每个 scene 一个文件,继续 colocate 数据结构 + 渲染函数.

### 3.2 props in,paint out(像 React 函数组件)

```rust
// "Props" 类型(命名约定:<Widget>Params 或 ScratchOnly struct)
pub struct SearchOverlayParams<'a> {
    pub overlay: &'a SearchOverlayView,
    pub inner_x: f32,
    pub inner_y: f32,
    pub grid_cols: u16,
    pub grid_rows: u16,
}

// "渲染"函数(约定:paint_<widget> 或 fn paint(&self, painter))
pub fn paint_search_overlay(p: &mut ViewPainter, params: SearchOverlayParams<'_>) {
    let view = View { rect: ..., style: ViewStyle { ... } };
    view.paint(p, |p| {
        // children
    });
}
```

scene 调用就是:

```rust
paint_search_overlay(&mut painter, SearchOverlayParams { ... });
```

跟 React `<SearchOverlay {...params} />` 同形.

### 3.3 hit_test 跟 paint 拆开

跟 React `<button onClick={...}>` 不同,我们暂时不抽事件总线.约定:

- widget 的 `layout()` 返回带 `rect`/`tab_rects`/`close_btn_rect` 字段的结果
- scene 自己保留这些 rect → 在 `mouse_down` 里调 widget 的 `hit_test` 或
  自己 `rect.contains(x, y)`

这跟 React 不一样(React 是 onClick-on-element),但 marspot 用 Metal,
没 DOM tree,这个折中是为了不过度抽象.将来如果 scene 多到值得引入,再考虑
事件总线.

## 4. 上层能力不足时

允许**两种**扩展方式,顺序如下:

### 4.1 优先:在现有 widget 上加 prop

例:`ModalFrame` 没考虑"可拖动",加 `pos_offset: (f64, f64)` 字段,scene
传进去.

例:`View` 没考虑 backdrop,加 `style.backdrop: Backdrop`.

加一条 prop 比新建组件便宜,scene 改一行就用上.

### 4.2 其次:新建 widget

下面这些情况新建文件:

- 跨平台风格(macOS / Windows / Linux)有差别 → `system/<platform>/<widget>.rs`
- 多个 scene 都用 → `components/<widget>.rs`
- 跟现有 widget API 形状差太多,塞 prop 反而模糊 → 新建

新 widget 文件**必须** 带:

1. file-level `//! ` doc-comment 说明这个 widget 是干嘛的
2. `layout()` 或构造方法 — 算 sub-rects + 状态
3. `paint(&self, &mut ViewPainter, ...)` — 用 ViewPainter,**不允许** 直接
   接收 `&mut Vec<CellInstance>` 等具体 scratch
4. 至少 3 个 unit test:
   - 默认布局
   - 边界情况(空、超长)
   - hit_test(如果有交互)

### 4.3 禁止:绕开 ui kit 直接写

scene 里面看到这两种情况之一就直接 reject:

```rust
ui_rects.push(...);                  // ✗ scene 不允许直接 push
let _ = layout.padding;              // (这是 layout system,不是 ui kit,例外)
```

scene 必须走 widget,widget 必须走 ViewPainter,ViewPainter 自己 push.

## 5. 现状 cheatsheet(2026-06)

| 路径 | 类型 | 用途 |
|---|---|---|
| `core/view.rs` | View / ViewStyle / ViewPainter / Backdrop | 任何 overlay 的基类 |
| `core/icon.rs` | IconComponent (trait) | 所有 icon 必须 impl 它 —— 没有闭包形 icon |
| `system/macos/traffic_lights.rs` | TrafficLights / TrafficLightHit | macOS 红黄绿 |
| `system/macos/title_bar.rs` | TitleBar / TitleBarHit | macOS 标题栏(traffic + 居中 title text) |
| `system/macos/icons/grid.rs` | GridIcon | Lucide layout-grid:外框 + 内部分隔线,参数化 cols×rows |
| `system/macos/icons/sidebar.rs` | SidebarIcon | Lucide panel-left:外框 + 1/3 处分隔线,collapsed 状态 dim |
| `system/macos/icons/list_tree.rs` | ListTreeIcon | Lucide list-tree:3 横条递进缩进 |
| `components/modal_frame.rs` | ModalFrame / ModalLayoutSpec | 中心 modal 几何(default / maximized / minimized / drag offset) |
| `components/tab_strip.rs` | TabStrip | 等宽 tab + ellipsis 截断 |
| `components/scroll_view.rs` | ScrollView | 垂直 scroll 状态 + wheel/clamp |
| `components/panel.rs` | Panel | View + 内边距 + content_rect() 自动 inset |
| `components/text_input.rs` | TextInput / TextInputStyle | 单行文本输入(值 + 光标 + 截断),paint-only |
| `components/list_view.rs` | ListView / ListRow / ListViewStyle | 垂直行列表 + focused 行高亮 + row_rect(i) 给 caller hit_test |
| `components/button.rs` | Button / ButtonStyle / IconSpec / IconPosition | 圆角按钮.支持 4 layout(Only/Before/After/text-only)、4 内置 style(default/ghost/destructive/chrome)、icon 两种 spec(Glyph 字符 / Component impl IconComponent).不接受 closure |
| `components/grid_seams.rs` | GridSeams / SeamStyle | x×y 网格的分隔线(inter-pane seams).vertical/horizontal 独立 SeamStyle{color, thickness},thickness=0 跳过.支持不均匀 cell —— seam 跨整个 grid 高/宽 |
| `components/search_overlay.rs` | paint_search_overlay / SearchOverlayParams | 用 Panel + TextInput + ListView 组合,F3+1.9 |

scene 代码:

| 路径 | 是什么 |
|---|---|
| `src/bin/marspot-core.rs:build_process_panel_render` | Process Monitor scene 的数据布局 + hit rect 计算 |
| `src/render_metal.rs:paint_process_panel_content` | Process Monitor scene 的内容渲染(用 ViewPainter) |
| `src/render_metal.rs:push_process_panel_via_view` | scene 入口(创建 View + 调 paint) |

将来 Process Monitor 文件多了可以单独抽 `src/ui/scenes/process_monitor.rs`.

## 6. 写完别忘了

- 跑 `cargo nextest run --lib` 看新 widget 的 unit test 全过
- 跑 `bin/test.sh` 看全套 test 还过
- 跑 `bin/bench-remote.sh` 看 render p99 没爆
- `default_style_is_opaque` test 没失败(意味着 ViewStyle.bg.alpha 还是 1.0)

## 7. 历史教训(为什么是这套规则)

| 时间 | 事 | 教训 |
|---|---|---|
| F1+11 | search overlay 发明 SDF rect,搞 1.5 个版本才不透 | 没架构层面隔离 overlay vs grid |
| F3+1.3++ | process panel 第一次显示透 | filter glyph-by-origin 不兜 extents |
| F3+1.5+ | 第二次显示透 | backdrop 盖了 marspot 标题条 |
| F3+1.6 | 第三次显示透 | filter 兜不住 cache 命中 / 边缘 / 宽字符 |
| F3+1.6 | overlay scratches + 多 pass 架构修复 | 真根因找到:Metal pipeline 顺序固定 |
| F3+1.7 | 抽 `View` 组件 | 让以后 scene 不可能再"忘记走 overlay" |
| F3+1.8 | 三层 ui kit + search overlay 迁移 | 把规则固化进文件夹结构,新人不会走错 |

**核心一句话**:**任何 marspot UI overlay 必须走 `core::View`,违反就是 bug.**
