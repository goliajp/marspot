# marspot UI 模型 — 完整设计 v3

> 2026-06-23 v3.  v2 自查发现 SOTA framework 的常用 primitive 缺了
> 不少(ScrollView / Image / Gesture 模型 / Identity-state 接通 /
> Lifecycle / Accessibility / ...).v3 补齐设计、明确每条的 v1 vs
> deferred 标记、给 implementation roadmap.

---

## 目录

- §0 设计原则
- §1 五层结构概览
- §2 L1 Foundation  — Length / Color / Tokens / Identity
- §3 L2 Box Model — padding / border / radius / shadow / opacity / clip
- §4 L3 Primitives — Canvas atoms (rect / line / text / image / gradient)
- §5 L4 Layout — View tree + Modifier chain + Containers
- §6 Constraints two-pass algorithm
- §7 Gesture model
- §8 Lifecycle + Stateful views
- §9 Accessibility
- §10 Animation (v2+)
- §11 Theme + Tokens
- §12 Rendering pipeline
- §13 L5 Components
- §14 Out of scope
- §15 Implementation roadmap
- §16 File structure
- §17 SOTA self-assessment
- §18 Reference comparison

---

## 0. 设计原则

1. **够 chrome 用就停**.marspot 不是浏览器,不需要完整 CSS.
2. **CSS 心智 + SwiftUI 命名 + Flutter 算法 + elm 状态流**.各取最强一面,不发明新词.
3. **声明式 View 树 + Modifier chain + immediate-mode 渲染**.每帧重建 View 树,无 retained,无 diff.
4. **submission order = z order**(已建立的不变量).
5. **统一 `Length`,统一 `View`,统一 `Modifier`**.没有第二套.
6. **Two-pass constraint layout**(Flutter / Compose 同形).
7. **ActionId-based interaction**(elm-y / serializable),不 closure-based.

---

## 1. 五层结构概览

```
┌──────────────────────────────────────────────────────┐
│  L5  Components    Card, Sheet, Modal, ContextMenu,  │
│                    Sidebar, Table, Panel, TabStrip,  │
│                    DevPanel, Tooltip, …              │
├──────────────────────────────────────────────────────┤
│  L4  Layout        View tree + Modifier chain +      │
│                    Containers + Gesture model        │
├──────────────────────────────────────────────────────┤
│  L3  Primitives    Canvas atoms: rect / line / text /│
│                    image / gradient / path           │
├──────────────────────────────────────────────────────┤
│  L2  Box Model     padding / border / radius /       │
│                    shadow / opacity / clip / mask    │
├──────────────────────────────────────────────────────┤
│  L1  Foundation    Length / Color / Tokens /         │
│                    Identity / State map              │
└──────────────────────────────────────────────────────┘
```

每层依赖下层,不反向.L4 是这份 doc 的新加点;L1-L3 已有基础需补.L5 在 L4 完备后整体重写.

---

## 2. L1 Foundation

### 2.1 `Length` — 单位

```rust
pub enum Length {
    Pt(f64),     // 绝对逻辑 pt(scale-independent)
    Pct(f64),    // 父轴小数比例(0.0..1.0)
    Ch(f64),     // chrome cell 宽倍数(terminal-domain)
}
```

| | CSS | SwiftUI | Flutter | marspot |
|---|---|---|---|---|
| 绝对 | `px` | `Length` literal | logical px | `Pt(N)` ✓ |
| 相对 | `%` | `Length::flexible` | `FractionallySizedBox` | `Pct(F)` ✓ |
| 字符宽 | `ch` | n/a | n/a | `Ch(N)` ✓ |

**v1 已落.** `Ch` 在 mono 字体下 = cell width,CSS `ch` 在比例字体下 = "0" 字符宽度 —— 终端域两者等价.

**砍** `Length::Auto` —— hug-content 走 `Option<Length>::None`(在 `FrameSpec.width` 等位置),不在 Length enum 里.viewport / em / rem 单位 marspot 不需要(单 NSWindow,单 font).

### 2.2 `Color`

```rust
pub struct Color { r: u8, g: u8, b: u8, a: f32 }
```

跟 CSS `rgba(r,g,b,a)` 一对一.**v1 已落.**

**砍**:HSL / OKLCH / color-mix / named-colors / system colors.

### 2.3 Tokens — 语义层(v1 部分落)

```rust
pub mod token {
    pub mod color  { /* fg / fg_muted / bg / bg_raised / accent / danger / ... */ }
    pub mod space  { pub const XS..XXL: Length }
    pub mod radius { pub const NONE / SM / MD / LG / PILL: Length }
    pub mod elev   { pub const E0 / E1 / E2 / E3: Shadow }      // 新加 §3.5
    pub mod text   { pub const Caption / Body / Header / LargeHeader: TextStyle }   // 新加 §5.4
    pub mod motion { pub const FAST / NORMAL / SLOW: Duration } // v2+
}
```

**已落**: `color::*` (18 项), `space::*` (6 档), `radius::*` (4 档).
**v1 应补**: `elev::*`(语义 elevation 阴影档,Material 风),`text::*`(语义文字 style 档).
**v2+**: `motion::*` 时长 token,配合 §10 Animation.

### 2.4 Identity / State map — **v1 必须接通**

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash)] pub struct ViewId(pub u32);

pub trait HostState {
    /// 拿 ViewId 对应的 stateful view 的 owned state(scroll
    /// offset / cursor pos / 选中索引 / 展开状态 / hover bit /
    /// focus bit / animation progress / ...).
    fn get<T: 'static>(&self, id: ViewId) -> Option<&T>;
    fn get_mut<T: 'static>(&mut self, id: ViewId) -> Option<&mut T>;
    fn insert<T: 'static>(&mut self, id: ViewId, v: T);
}
```

**v2 漏了什么**: `Modifier::Id(ViewId)` 只声明了,host 没 state map.于是 ScrollView / TextField / Toggle 这类 stateful view 无处寄存状态.

**v1 必须做**:
- ShellApp 持 `view_state: HashMap<ViewId, Box<dyn Any>>`
- View 类型(下方提的 ScrollView / TextField 等)layout 时按 `id` 读 state,生 view 时按 state 决定渲染
- ActionId 路由的事件 reducer 可以 `host.state.get_mut<ScrollState>(id).offset_y = ...`

参照: SwiftUI `@State`(隐式 id 绑 parent)/ Compose `remember{}`(slot table)/ React `useState`(hook).我们走显式 `ViewId` —— 显式 = 可序列化 / 可单测 / 没 hidden magic.

---

## 3. L2 Box Model

### 3.1 Border-box 永远(已落)

`.size(w, h)` 量外缘.跟 CSS `box-sizing: border-box` 一致.

### 3.2 没有 margin(已落)

margin collapse 是公认设计错误,SwiftUI/Compose/Flutter 都没.我们继承.

### 3.3 Inside-stroke border(已落)

border 内描边,不撑大尺寸.

### 3.4 完整 modifier 表 — 补齐版

| Modifier | 现状 | v1 必须 | 说明 |
|---|---|---|---|
| `.padding(Edges)` | ✓ 已落 | ✓ | inset content |
| `.background(Color)` | ✓ 已落 | ✓ | solid fill |
| `.background(Gradient)` | ✗ | **v1 补** | 线性渐变;chrome polish |
| `.background_material(Material)` | ✗ | **v1 补** | macOS NSVisualEffectView 包裹(vibrancy) |
| `.border(Length, Color)` | ✓ 已落 | ✓ | inside-stroke |
| `.corner_radius(Length)` | ✓ 已落 | ✓ | |
| `.shadow(Shadow)` | ✓ 已落 | ✓ | 单 shadow;多层走 token `elev::E*` |
| `.opacity(f64)` | ✗ | **v1 补** | 子树整体透明度(乘所有 child alpha) |
| `.clip(ClipShape)` | ✗ | **v1 补** | rect / 圆角 rect 裁剪(用于 ScrollView / image fit) |
| `.mask(View)` | ✗ | v2+ | 用 view 作 alpha mask;少用,留 slot |
| `.frame(FrameSpec)` | ✓ 已落 | ✓ | width / height / min / max / aspect / align |
| `.aspect_ratio(f64, ContentMode)` | △ 声明未实施 | **v1 补** | 保持 w/h 比;ContentMode = Fit | Fill |
| `.offset(Length, Length)` | ✓ 已落 | ✓ | 主要给 ZStack 用 |
| `.z_index(i32)` | △ 声明未读 | **v1 补** | ZStack 内重排 z 顺序;否则跟 submission order |
| `.hidden(bool)` | △ 部分 | **v1 补** | 区分两种:`.hidden(true)` = 占空间不画(visibility:hidden);`.collapsed(true)` = 不占空间(display:none) |
| `.on_hover(HoverId)` | △ 注册未驱动 | **v1 补** | 配合 §7 Gesture |
| `.on_click(ActionId)` | ✓ 已落 | ✓ | |
| `.on_double_click(ActionId)` | ✗ | **v1 补** | |
| `.on_right_click(ActionId)` | ✗ | **v1 补** | |
| `.on_drag(DragHandler)` | ✗ | **v1 补** | begin / move / end 三阶段 — §7 |
| `.on_scroll(ScrollHandler)` | ✗ | **v1 补** | 滚轮事件落到当前 view |
| `.on_key(KeyHandler)` | ✗ | v2+ | view 局部键盘事件 |
| `.shortcut(KeyEquivalent, ActionId)` | ✗ | v2+ | Cmd-shortcut 注册 |
| `.focusable(FocusId)` | ✗ | v2+ | 加入 Tab 导航环 |
| `.accessibility_label(&str)` | ✗ | **v1 留 slot** | 不实施但声明,AX 后续补 |
| `.id(ViewId)` | △ 声明未接 | **v1 补**(配合 §2.4 state map) | 稳定身份 |
| `.transition(Transition)` | ✗ | v2+ | 进入 / 离开过渡(配合 §10 Animation) |

**应用顺序**:modifier chain 从内到外 wrap.`.padding().background()` —— 背景 outside padding;`.background().padding()` —— 背景 inside padding.SwiftUI / Compose 都这样.

---

## 4. L3 Primitives — Canvas atoms

### 4.1 现有(已落)

`Canvas` 内部 `Primitive` 队列:`Rect / Line / Text`,提交顺序 = z 顺序.

### 4.2 v1 必须补

**`Image`** —— 现在 chrome 图标是 hard-coded SDF,toolbar / icon button 没法用 view 树声明.

```rust
pub enum ImageSource {
    /// Glyph atlas entry (renderer 现有 path)
    Glyph(GlyphRef),
    /// Inline RGBA bytes (PNG 解码后);Cache by Rc<...> id 避免重传 GPU
    Raw(Rc<RawImage>),
    /// IOSurface ref(将来支持外部图像)
    IOSurface(u32),
}

pub struct Image {
    pub source: ImageSource,
    pub content_mode: ContentMode,   // Fit / Fill / Center
    pub tint: Option<Color>,
}
```

**`Gradient`** —— LinearGradient + 后续 RadialGradient.填充 / 背景用,chrome polish 关键.

```rust
pub enum Fill { Solid(Color), Linear(LinearGradient) }
pub struct LinearGradient {
    pub stops: Vec<(f64, Color)>,    // (0.0..1.0, color)
    pub direction: GradientDir,      // TopBottom / LeftRight / 自定义 angle
}
```

**`Shape`** primitive — 自定义 path(SVG-like beziers).chrome icon 用 SDF 或 path 走这条.声明 enum,实现可以后续(v1 先支持 Rect / RoundedRect / Circle / Capsule;复杂 path 留 v2+).

### 4.3 v2+

- **Path** — 任意 bezier
- **Blur backdrop** — Metal pipeline 加 blur kernel,跑 vibrancy
- **Mask compose** — view as alpha mask

---

## 5. L4 — View 树

### 5.1 View enum — 完整版

```rust
pub enum View {
    // ─── Atoms ─────────────────────────────────────
    Text(Text),
    Spacer(Spacer),
    Filled(Filled),
    Hairline(Hairline),
    Divider(Divider),       // ← 新:semantic;比 Hairline 多 padding / style
    Image(Image),           // ← 新
    Shape(Shape),           // ← 新:Circle / Capsule / RoundedRect / Path
    Canvas(CanvasRef),      // 逃生舱

    // ─── Containers ────────────────────────────────
    VStack(VStack),
    HStack(HStack),
    ZStack(ZStack),
    Grid(Grid),             // ← 新:m×n;不夸张实现,Track auto/fixed/flex
    ScrollView(ScrollView), // ← 新:clip + offset + 滚动
    LazyVStack(LazyVStack), // ← 新:虚拟化长列表
    LazyHStack(LazyHStack), // ← 新

    // ─── Stateful (需要 ViewId state map) ──────────
    TextField(TextField),   // ← 新:文本输入
    Toggle(Toggle),         // ← 新:on/off switch
    Picker(Picker),         // ← 新:dropdown / segmented control

    // ─── Modified (modifier chain 内部表示) ────────
    Modified { child: Box<View>, mods: Vec<Modifier> },
}
```

### 5.2 容器细节 — 新加的

**`ScrollView`** ——

```rust
pub struct ScrollView {
    pub child: Box<View>,
    pub direction: ScrollDir,       // Vertical / Horizontal / Both
    pub id: ViewId,                 // ⚠ stateful — host 持 ScrollState
    pub shows_indicators: bool,
}

pub struct ScrollState {            // host map 里
    pub offset: (f64, f64),         // 当前 scroll 偏移(phys)
    pub content_size: (f64, f64),   // 上次 layout 报的 content 总尺寸
}
```

Layout: child 在 unbounded 主轴上 layout,实际显示走 clip + offset.滚轮事件改 offset.
Paint: 渲染前 push clip rect,paint child,pop clip.

**`LazyVStack` / `LazyHStack`** ——

```rust
pub struct LazyVStack {
    pub items: Vec<View>,   // 调用方先准备好 — v1 不做真懒(lazy fn)
    pub estimated_height: Length,
    pub gap: Length,
    pub id: ViewId,
}
```

v1 取巧:items 仍预生成,但 layout 只跑可见区(基于 ScrollState offset).真"lazy fn" 留 v2+(需要 dyn trait 或 macro).

**`Grid`** —— 简化 CSS Grid,只支持 fixed track + auto track:

```rust
pub struct Grid {
    pub children: Vec<View>,
    pub rows: Vec<GridTrack>,        // Pt(L) | Auto | Flex(N)
    pub cols: Vec<GridTrack>,
    pub gap: (Length, Length),       // (col_gap, row_gap)
}
```

放弃 row-span / col-span(用 nested stack 替代);放弃 line-names / template-areas.

### 5.3 Modifier chain(已落 + v1 补)

见 §3.4.

### 5.4 `Text` 完整 — 形式化字型 token

```rust
pub struct Text {
    pub content: String,
    pub style: TextStyle,    // ← 形式化,不再 size+weight+color 散
    pub align: TextAlign,
    pub lines: TextLines,
}

pub struct TextStyle {
    pub size: TextSize,
    pub weight: TextWeight,
    pub color: Color,
    pub italic: bool,
}
```

`TextStyle` 提到 `token::text::*`:
```rust
pub mod text {
    pub const Caption:     TextStyle = TextStyle { size: Caption, weight: Regular, color: FG_MUTED, ... };
    pub const Body:        TextStyle = ...;
    pub const Header:      TextStyle = ...;
    pub const LargeHeader: TextStyle = ...;
    pub const Code:        TextStyle = TextStyle { size: Body, weight: Regular, color: ACCENT_DIM, ... }; // 等宽强调
}
```

调用方写 `Text::new("hi").style(token::text::Header)`,不再每次组合 size + weight + color.

**修 v2 hack**:`TextWeight::Dim = alpha × 0.6` 退掉.Dim 改成 `style: text::Caption`(用 FG_MUTED color)或显式 `.color(...)`.

---

## 6. Constraints two-pass algorithm

(同 v2,已落.补一个之前漏说的不变量)

### 6.1 新加不变量

- **Aspect ratio 优先级**:同时给 `width` + `height` + `aspect_ratio` 时,aspect_ratio 被忽略(显式 size 赢).只给 aspect_ratio + 一个维度时,推算另一维.
- **Lazy 容器特例**:LazyVStack 的 layout 只跑当前 visible window 的 children,其余按 `estimated_height` 占位.
- **Stateful view 特例**:layout 前从 `HostState` 读 ScrollState / TextField cursor / ...,把它们参与 layout 计算.

### 6.2 Intrinsic size queries(v1 补)

Flutter 有 `IntrinsicWidth` / `IntrinsicHeight`,query child 的 preferred 尺寸.我们 v1 加一个简化版:

```rust
impl View {
    /// 在不绑定 layout pass 的情况下查 self 的 intrinsic size.
    /// 用于 parent 在算 main / cross 时不影响 child 真正 layout 的 query.
    pub fn intrinsic_size(&self, ctx: LayoutCtx) -> Size;
}
```

实现 = 跑一次 `layout(self, ctx, (0,0), Constraints::loose(INF, INF))`,记录 rect.

---

## 7. Gesture model — 新章

### 7.1 事件类型

```rust
pub enum InputEvent {
    Click(Point),
    DoubleClick(Point),
    RightClick(Point),
    DragBegin(Point),
    DragMove { from: Point, to: Point, delta: (f64, f64) },
    DragEnd { from: Point, to: Point },
    Hover { at: Point, entered: bool },
    Scroll { at: Point, delta: (f64, f64), precise: bool },
    KeyDown(KeyCode, Modifiers),
    KeyUp(KeyCode, Modifiers),
}
```

### 7.2 路由

view 树 hit_test 走 §8 同形(post-order, deepest+topmost wins).每个 InputEvent → ActionId → reducer.

```rust
fn hit_test_event(laid: &LaidOut, ev: &InputEvent) -> Option<ActionId>;
```

Modifier 注册:
- `.on_click(id)` — 普通 click
- `.on_double_click(id)` — 双击
- `.on_right_click(id)` — 右键(已有 marspot ContextMenu 路径)
- `.on_drag(DragHandler)` — drag 三阶段;返三个 ActionId(begin/move/end)
- `.on_scroll(handler)` — 滚轮在 view 上时触发
- `.on_hover(hover_id)` — 跟 mouse-move 路径联动,host 持 `hover_id: Option<HoverId>`

### 7.3 drag 状态机

drag 跨多个 event(down + move × N + up).host 持 `drag_state: Option<DragInProgress>`.layout 期间 hit_test 锁定 begin 的 view,后续 move/up 不重 hit_test —— 这是 standard drag behavior(SwiftUI / AppKit / browser DOM 都这样).

### 7.4 Keyboard

**v1 不做** view 局部键盘事件;主窗口 key dispatch 仍在 `app::EventKind::Key` 这条主路径.modal/input field 这种需要 view 局部 key 的,等真做 TextField 时一起补.

---

## 8. Lifecycle + Stateful views — 新章

### 8.1 Lifecycle hooks

immediate mode + retained state map 需要"view 何时存在 / 何时消失"信号:

- **首次出现** — host state map 里没有这个 ViewId,但本帧 view 树有
- **持续存在** — id 在 map 跟 tree 都有
- **消失** — id 在 map 有,但本帧 tree 没

```rust
pub trait LifecycleHook {
    fn on_appear(&mut self, ctx: &mut HostState);
    fn on_disappear(&mut self, ctx: &mut HostState);
}

// 每帧 build_view 跑完,framework 自动 reconcile:
//   new_ids = tree.collect_ids()
//   for id in old_ids - new_ids: state.on_disappear(); state.remove(id);
//   for id in new_ids - old_ids: state.on_appear();
```

**v1 必须做** —— ScrollView / TextField 状态需要在 disappear 时清掉,否则 memory leak.

### 8.2 Stateful Views — v1 加哪些

| View | 状态 | 用例 |
|---|---|---|
| `ScrollView` | offset, content_size | 任何长列表 |
| `LazyVStack` | offset(继承 parent ScrollView)| 长列表虚拟化 |
| `TextField` | text, cursor_pos, selection, focused | 重命名 / 输入 |
| `Toggle` | on (bool) | settings |
| `Picker` | selected_idx | dropdown / segmented |

**只做 v1 真用得到的**:ScrollView + TextField.其余先声明,实施按 demand.

### 8.3 上游 host 集成

ShellApp 加:
```rust
view_state: HashMap<ViewId, Box<dyn ViewState>>,
hover: Option<HoverId>,
focus: Option<FocusId>,
drag: Option<DragInProgress>,
```

每帧:
1. `build_view(&host_state)` → View tree
2. `layout(tree, ctx, ...)` → LaidOut tree(从 state map 读 ScrollState 等)
3. paint
4. event-loop 进来 event → `hit_test_event(laid, ev)` → ActionId → reducer 改 host state
5. `reconcile_state(laid, &mut host_state)`(跑 appear/disappear)
6. `request_redraw()`

---

## 9. Accessibility — 新章(留 v2 接口)

**v1 不实施**,但接口 reserve:

- `.accessibility_label(&str)` modifier — view 给出的 AX 文本
- `.accessibility_role(AxRole)` — Button / Heading / ListItem / TextField / Image / ...
- `.accessibility_traits(AxTraits)` — Selected / Disabled / FocusedSection / ...

实施时绑 `objc2-app-kit::NSAccessibility`,把 LaidOut tree 翻译成 AX tree.v1 写 `Modifier::AccessibilityLabel(String)` enum,layout 时 bake 进 LaidOut,paint 时忽略.AX dispatch 路径整个留 v2+.

---

## 10. Animation(v2+)

(同 v2)

v1 完全不做.架构留空间走 time-based Anim<T>:

```rust
pub struct Anim<T> { from: T, to: T, started_at: Instant, duration: Duration, curve: Curve }
```

加 `.transition(t: Transition)` modifier —— 控制 view 进入 / 离开时的视觉 transition.implement 时,host 持有 anim 列表,layout / paint 看是否要插值.

---

## 11. Theme + Tokens

### 11.1 v1 加 ThemeId 全局

```rust
pub enum ThemeId { Dark, Light, HighContrast }

pub fn theme() -> &'static Theme;
pub fn set_theme(id: ThemeId);
```

v1 只实现 Dark.token module 内部根据 active ThemeId 切换常量.set_theme 触发全 redraw.

### 11.2 token 完整清单

跟 §2.3 一致.关键加:
- `elev::E0..E3` — Material 风 elevation shadow 档
- `text::*` — TextStyle 语义档(§5.4)
- `motion::FAST/NORMAL/SLOW` — duration 档(v2+)

---

## 12. Rendering pipeline

```
host state
   ↓
build_view(state)              ← 每帧调,返 View tree
   ↓
layout(view, ctx, constraints) ← Constraints two-pass
   ↓ LaidOut tree (rect / deco / hit_regions)
reconcile_state(laid)          ← appear / disappear hook
   ↓
paint(laid) → Canvas           ← submission order = z order
   ↓
encode_canvas(canvas, encoder) ← GPU
```

**每帧重建整树**,O(N) build + O(N) layout + O(N) paint + O(N) reconcile.marspot chrome 规模(几百节点)< 1ms.省 retained tree / diff / reconciliation 的复杂度,净赚.

---

## 13. L5 Components

V1 必有(在重写顺序上从浅到深):

| Component | 何时迁 | 难度 | 状态 |
|---|---|---|---|
| **DevPanel.Model section** | now | ★ | ✓ 已迁(0.6.13)|
| **TabStrip** | P3i.1 | ★ | 待 |
| **ContextMenu** | P3i.2 | ★★ | 已用 Canvas,待迁 View 树 |
| **Tooltip**(新)| P3i.3 | ★★ | hover-trigger popup |
| **Card** / **Panel**(新 preset)| P3i.4 | ★ | 单纯 modifier 组合 |
| **Sidebar** | P3i.5 | ★★ | ScrollView + 行 click |
| **Table** | P3i.6 | ★★★ | LazyVStack + 列定义 |
| **LayoutModal** | P3i.7 | ★★ | Modal + card grid + drag |
| **DevPanel 主框架** | P3i.8 | ★★ | tab strip + menu + content area |
| **ProcessMonitor** | P3i.9 | ★★★ | Table + 实时数据 |
| **SearchOverlay** | P3i.10 | ★★ | TextField + LazyVStack |

ViewPainter 退役 = 上面全迁完之后顺手做的事.

---

## 14. 明确不做

| 不做 | 原因 |
|---|---|
| 完整 CSS Grid(template-areas / line-names / span)| 用嵌套 stack 替代 |
| `flex-wrap` / `order` / `flex-basis` | 增 layout 复杂度,chrome 用例少 |
| `margin`(含 negative) | margin collapse 公认错误 |
| `position: sticky / fixed` | 终端 chrome 没用 |
| CSS animation / transition / keyframes | v2+ Anim<T> 路径 |
| CSS transform / matrix3d | 没场景 |
| Pseudo-classes / -elements | host state + modifier 模拟 |
| Media queries | 单 NSWindow,直接 Pct |
| 响应式 / signals / observable | immediate 够 |
| Virtual DOM / diff / reconciliation | 每帧重建,N 小,免费 |
| 多 font-family / icon font | SDF + glyph atlas 已覆盖 |
| 多 theme JSON / TOML | 自用阶段没人配 |
| 多语言 / RTL / unicode bidi | v1 中英文 LTR 够;Leading/Trailing 命名留 RTL 接口 |
| Subpixel font hinting | macOS 默认 grayscale |
| Print stylesheet | (笑)|
| 3D transform / perspective | 没场景 |
| Custom GLSL shader per view | 渲染层粒度对不上 |

---

## 15. Implementation roadmap

按 effort × 收益,P3a-h+k 已落 (~1500 LOC);v3 新加阶段:

| 阶段 | 内容 | LOC | 阻塞 | 状态 |
|---|---|---|---|---|
| **P3a** | token module | ~200 | — | ✓ |
| **P3b** | Length::Ch | ~80 | — | ✓ |
| **P3c** | View enum + Modifier 基础 + Edges/Anchor/FrameSpec | ~250 | P3a | ✓ |
| **P3d-f** | Constraints + VStack/HStack/ZStack + Spacer | ~750 | P3c | ✓ |
| **P3g-h** | paint pass + hit_test_click | ~290 | P3d | ✓ |
| **P3k** | theme entrypoint | ~50 | P3a | ✓(基础) |
| **P3l** | TextStyle 形式化 + token::text::* | ~120 | P3c | 待 |
| **P3m** | Opacity / Clip / Aspect ratio modifier 实施 | ~200 | P3d | 待 |
| **P3n** | Gesture model — DragBegin/Move/End + Hover state | ~280 | P3h | 待 |
| **P3o** | HostState `view_state` map + reconcile | ~200 | P3c | 待 |
| **P3p** | ScrollView + ScrollState + scroll event 路由 | ~350 | P3n+o | 待 |
| **P3q** | Image / Gradient fill / Material(macOS vibrancy) | ~400 | P3c | 待 |
| **P3r** | LazyVStack/HStack(简版,基于 ScrollView) | ~250 | P3p | 待 |
| **P3s** | TextField + Toggle + Picker(stateful)| ~500 | P3o+n | 待 |
| **P3t** | Lifecycle on_appear / on_disappear | ~150 | P3o | 待 |
| **P3u** | Accessibility label / role / traits stub(留 v2 接口) | ~100 | P3c | 待 |
| **P3v** | ThemeId 全局 + Light theme tokens | ~150 | P3k | 待 |
| **P3i.1-10** | 10 个 component 迁到 View 树 | ~1500 | 上面足够多落 | 待 |
| **P3j** | ViewPainter 退役 | -400 净删 | P3i 完 | 待 |

**总 v3 新加**:~3000 LOC + ~1500 LOC component 迁移 - 400 LOC 删 = 净 +4100 LOC.分批 2-3 周完成.

**优先级**:P3l-o 是"完整 stateful view + state map"的核心,必须打通;P3p (ScrollView) 是马上要用的;P3q (Image / Material) 是 chrome polish 必须;P3r-s 按 demand 跟 component 迁移.

---

## 16. 文件结构(v3)

```
src/ui/
├── core/                       ← L1 + L3
│   ├── units.rs                Length (Pt / Pct / Ch)
│   ├── color.rs                Color
│   ├── canvas.rs               Canvas (Primitive: Rect/Line/Text/Image/Gradient/Shape)
│   ├── image.rs                ← 新:ImageSource / ContentMode
│   ├── gradient.rs             ← 新:LinearGradient / RadialGradient
│   ├── shape.rs                ← 新:Circle / Capsule / RoundedRect / Path
│   └── mod.rs
├── theme/                      ← L1 token
│   ├── token.rs                color / space / radius / elev / text / motion
│   ├── dark.rs                 Dark theme(v1)
│   ├── light.rs                Light theme(v2+)
│   └── mod.rs
├── view/                       ← L4
│   ├── view.rs                 enum View + fluent helpers
│   ├── modifier.rs             Modifier enum
│   ├── types.rs                Edges / FrameSpec / Anchor / AlignCross / Distribute / Shadow
│   ├── ids.rs                  ActionId / HoverId / FocusId / ViewId
│   ├── stack.rs                VStack / HStack / ZStack
│   ├── grid.rs                 ← 新:Grid 简化版
│   ├── scroll.rs               ← 新:ScrollView + ScrollState
│   ├── lazy.rs                 ← 新:LazyVStack / LazyHStack
│   ├── text.rs                 Text + TextStyle
│   ├── image.rs                ← 新:Image view
│   ├── input.rs                ← 新:TextField / Toggle / Picker
│   ├── divider.rs              ← 新:semantic Divider
│   ├── constraints.rs          Constraints + layout pass
│   ├── paint.rs                paint pass
│   ├── hit_test.rs             hit-test (click / right_click / drag / hover / scroll)
│   ├── gesture.rs              ← 新:InputEvent / DragInProgress / 状态机
│   ├── lifecycle.rs            ← 新:reconcile_state(LaidOut, &mut HostState)
│   └── mod.rs
├── components/                 ← L5(P3i 整体重写)
│   ├── dev_panel.rs            ✓ Model section 已迁,主框架 P3i.8
│   ├── context_menu.rs
│   ├── layout_modal.rs
│   ├── table.rs
│   ├── sidebar.rs
│   ├── tab_strip.rs
│   ├── tooltip.rs              ← 新
│   ├── card.rs                 ← 新 preset
│   └── mod.rs
└── system/macos/               ← AppKit 桥
    ├── accessibility.rs        ← 新:NSAccessibility 绑定(v2+)
    ├── material.rs             ← 新:NSVisualEffectView 绑定(P3q)
    ├── traffic_lights.rs       (现有)
    └── ...
```

---

## 17. SOTA self-assessment(更新版)

| 维度 | SwiftUI | Compose | Flutter | iced | egui | marspot v3 plan |
|---|---|---|---|---|---|---|
| API 形态 | View struct + modifier | Composable + modifier | Widget + 嵌套 | fluent + closure | imperative | ✓ View enum + modifier chain |
| 树形 | retained + diff | retained + diff | retained + diff | immediate | immediate | immediate(取舍:简单 + serializable) |
| 状态 | @State property | remember{} slot table | InheritedWidget | closure capture | id-scoped | ✓ ActionId reducer + HostState map(elm-y) |
| 动画 | withAnimation 自动 | animateAsState | AnimationController | n/a 弱 | n/a | v2+ Anim<T> |
| Layout | Constraint propagation | Constraints two-pass | Constraints two-pass | flexbox-y | layout fn | ✓ Constraints two-pass(Flutter 同) |
| Scroll | ScrollView | LazyColumn | ListView | scrollable | ScrollArea | **P3p 待补** |
| Image | Image / SF Symbol | painterResource | Image.asset | n/a 弱 | image | **P3q 待补** |
| Gesture | gesture modifiers | Modifier.pointerInput | GestureDetector | Subscription | InputState | **P3n 待补**(本框架最弱处) |
| AX | 自动 + manual | 自动 + manual | 自动 + manual | 弱 | 弱 | **stub 留 slot,v2+ 实施** |
| Theme | ColorScheme env | MaterialTheme | Theme | wgpu palette | Visuals | ✓ token + ThemeId |
| Material backdrop | .background(.regularMaterial) | n/a iOS-only | n/a | n/a | n/a | **P3q 必须**(macOS chrome 关键) |

**结论 v3**:
- **Layout 算法 / View 树 / Modifier chain / State 流** —— 跟 SwiftUI/Compose/Flutter 持平
- **Scroll / Image / Gesture** —— v2 缺,v3 补,补齐后跟它们持平
- **Animation / Accessibility** —— v2+;留接口
- **immediate-mode + 每帧重建** —— 跟 iced/egui 同;放弃自动 state 延续 + 自动动画 = 有意识取舍
- **跟 CSS / DOM** —— 拿心智,砍实现复杂度;现代框架共识方向

**For our scope, SOTA.** 唯一未到 SOTA 的是 Gesture(P3n 补)、ScrollView(P3p 补)、Image(P3q 补);这些不是创新空白,是抄结论的事.

---

## 18. Reference comparison

| 系统 | 抄了 | 没抄 |
|---|---|---|
| CSS | 单位 / box model / rgba / 心智 | 完整 spec / Grid / animation / 选择器 / cascading / 伪类 |
| SwiftUI | View enum + modifier chain / Anchor / @State 概念(改 elm)/ Frame / Padding 命名 | property wrapper / Combine / 自动状态绑定 / 自动动画 |
| Jetpack Compose | Modifier 链 / Constraints / LazyColumn 概念 | Recomposer / Kotlin coroutines / Slot table |
| Flutter | Constraints two-pass / Widget 命名(改 View)/ Spacer / Expanded | Widget retained tree / RenderObject / GestureDetector closure / Provider |
| iced / egui | 立即模式精神 | message-bus / 完整 widget 库 |

---

## 19. 一句话(updated)

**marspot UI v3 = CSS 心智 + SwiftUI 命名 + Flutter Constraints 算法 + immediate-mode 渲染 + elm/redux 状态流 + 完整 stateful view 集(ScrollView / TextField / Toggle / Picker / LazyVStack)+ 完整 gesture(Click/Drag/Hover/Scroll/Shortcut)+ macOS Material backdrop + Accessibility stub 留 v2 接口.**

跟所有现代 UI framework 的 SOTA 取舍 align;砍掉不适用 marspot scope 的 reactivity / 完整 CSS / 自动动画 / 多 theme / i18n / AX 实施.最终 ~5000 LOC framework 长期养 marspot 所有 chrome.
