# marspot UI 模型 — 完整设计 v2

> 2026-06-23 起草,polish 一遍把 v1 hand-wavy 的地方顶到现代框架
> 的 state-of-art。
>
> 这是 marspot UI 系统的**唯一**长期设计 doc。一个 1500-2000 LOC
> 的 framework,够 chrome 类 UI(title strip / sidebar / modal /
> tab strip / dev panel)长期用,不奢望做通用 GUI。
>
> 参照系:CSS / SwiftUI / Jetpack Compose / Flutter / iced。每条
> 都标注**抄了谁、砍了谁、为什么**。

---

## 0. 设计原则(总纲)

1. **够 chrome 用就停**。砍掉的比留下的重要,见 §19。
2. **CSS 心智 + SwiftUI 命名 + Flutter 算法**。各取最强一面,
   不重新发明术语。
3. **声明式 View 树 + Modifier chain + immediate-mode 渲染**。
   View 树是 build 时的临时结构,每帧重建,不 retained,无 diff。
4. **submission order = z order**(已建立的不变量)。
5. **统一长度类型 `Length`**,统一容器形态 `View`,统一附加属性形态
   `Modifier`。**没有第二套**。
6. **Two-pass constraint layout**(参 Flutter / Compose)—— 父往下
   传 constraints,子往上返 size。
7. **不发明新名词**。`VStack`/`HStack`/`Spacer`/`Padding` 这些
   词已经统一,直接用。

---

## 1. 五层概览

```
┌──────────────────────────────────────────────────┐
│  L5  Components    Tabs, Modal, Menu, Panel, ... │
├──────────────────────────────────────────────────┤
│  L4  Layout        View tree + Modifiers +       │
│                    Constraints two-pass          │
├──────────────────────────────────────────────────┤
│  L3  Primitives    Canvas: rect / line / text    │
├──────────────────────────────────────────────────┤
│  L2  Box model     fill / border / radius /      │
│                    shadow / padding              │
├──────────────────────────────────────────────────┤
│  L1  Foundation    Length / Color / Tokens       │
└──────────────────────────────────────────────────┘
```

L1-L3 已落地。**L4 是这份 doc 的新区**,L5 在 L4 落地后整体重写。
明确分工:
- L3 (Canvas) 是**绘画**层,接 GPU。
- L4 (Layout) 是**摆位**层,纯几何计算,不画。
- L4 落到 L3 的唯一方式 = layout 树跑完后 paint pass 把每个节点
  对应的 primitives 提交进 Canvas。

---

## 2. L1 Foundation

### 2.1 `Length` — 单位

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// 绝对逻辑 pt(scale-independent).Pt(1.0) ≡ CSS 1px.
    Pt(f64),
    /// 父轴的小数比例(0.0..1.0).Pct(0.5) ≡ CSS 50%.
    Pct(f64),
    /// chrome cell 宽度的 N 倍.Ch(1) ≡ CSS 1ch.终端域专用.
    Ch(f64),
}
```

| | CSS | SwiftUI | Flutter | marspot |
|---|---|---|---|---|
| 绝对 | `px` | `Length` literal | logical px | `Pt(N)` |
| 相对 | `%` | `Length::flexible` | `FractionallySizedBox` | `Pct(F)` |
| 字符宽 | `ch` | n/a | n/a | `Ch(N)` |
| viewport | `vw/vh` | `GeometryReader` | `MediaQuery` | n/a(走 Pct,只一个 window)|
| em / rem | font-relative | `@ScaledMetric` | textScaleFactor | n/a(单 font cell)|

**v1 取消 `Length::Auto`** —— hug-content 走 `Option<Length>::None`,
不在 Length 内部表达。Length 是**值**(resolve 出一个 f64),Auto
是**意图**(让 layout 自己决定),两个层级不该混。

### 2.2 `Color`(已落)

`Color::rgba(r: u8, g: u8, b: u8, a: f32)`,等价 CSS `rgba(...)`。
**不加**:HSL / OKLCH / color-mix / named-colors。

### 2.3 Tokens — 语义层

```rust
pub mod token {
    pub mod color {
        pub const fg:           Color = …;  // 主前景
        pub const fg_muted:     Color = …;  // 次前景
        pub const fg_disabled:  Color = …;
        pub const bg:           Color = …;
        pub const bg_raised:    Color = …;  // panel / modal 背景
        pub const bg_selected:  Color = …;  // 选中态
        pub const bg_hover:     Color = …;
        pub const border:       Color = …;
        pub const divider:      Color = …;
        pub const accent:       Color = …;
        pub const danger:       Color = …;
        pub const shadow:       Color = …;
    }
    pub mod space {
        pub const XS: Length = Length::Pt(4.0);
        pub const SM: Length = Length::Pt(8.0);
        pub const MD: Length = Length::Pt(12.0);
        pub const LG: Length = Length::Pt(16.0);
        pub const XL: Length = Length::Pt(24.0);
        pub const XXL:Length = Length::Pt(32.0);
    }
    pub mod radius {
        pub const SM: Length = Length::Pt(3.0);
        pub const MD: Length = Length::Pt(6.0);
        pub const LG: Length = Length::Pt(10.0);
        pub const PILL: Length = Length::Pt(9999.0); // 等价 9999px
    }
}
```

**规则**:组件代码用 `token::color::accent`,**不**直接写
`Color::rgba(91, 162, 250, 1.0)`。

参照:CSS Custom Properties / Tailwind Theme / Material Design Tokens
/ SwiftUI Semantic Colors。这是现代设计系统的最低标准,不做就跟
不上。

---

## 3. L2 Box Model

### 3.1 Border-box,永远

```
┌─ size(w, h) ────────────────────────┐
│  ┌─ border ────────────────────┐    │
│  │  ┌─ padding ──────────────┐ │    │
│  │  │     content rect       │ │    │
│  │  └────────────────────────┘ │    │
│  └─────────────────────────────┘    │
└─────────────────────────────────────┘
```

`.size(w, h)` 量的永远是**外缘**(等价 CSS `box-sizing: border-box`)。
**不支持 content-box**(CSS 原始默认,太容易踩坑,现代工具链全
border-box)。

### 3.2 没有 margin

`margin` 是 CSS 的历史包袱(margin collapse 是公认设计错误)。
现代框架(SwiftUI / Compose / Flutter)都**没有 margin**,只有
父级容器的 padding 或 `gap`。我们继承这条:

| 想做的事 | 怎么做 |
|---|---|
| 元素自己有外间距 | 父容器加 `padding` 或 `gap` |
| 单边外间距 | 父用 HStack/VStack + `Spacer` |
| 全屏 padding | 父级 `Pad` 包一层 |

### 3.3 Inside-stroke border

已实现:`border(width, color)` 是从内侧吃掉 width 像素,size 不变。
跟 web / SwiftUI / Compose 现代行为一致(CSS 默认 outside-stroke
是历史遗留,box-sizing: border-box 之后等价 inside)。

---

## 4. L3 Primitives — Canvas(已落)

不重复 RFC。只补一点:**Canvas 不再是组件的 public API**。
组件返 View 树,framework 走 layout + paint 两阶段把 View 翻译
成 Canvas primitives。Canvas 退到**底层接口**,组件代码大多不直
接见 Canvas。

---

## 5. L4 — View 树

### 5.1 `View` 是什么

```rust
pub enum View {
    // ─── Atoms (叶子) ───
    Text(Text),
    Spacer(Spacer),
    Filled(Filled),    // 纯色矩形(底层)
    Hairline(Hairline),// 单像素分隔线
    Canvas(CanvasRef), // 逃生舱:直接画 Canvas primitives

    // ─── Containers (有 children) ───
    VStack(VStack),
    HStack(HStack),
    ZStack(ZStack),

    // ─── Modified (modifier chain 内部表示) ───
    Modified(Modified),
}
```

View 树是**临时数据结构**,每帧 build 一次,layout + paint 完就丢。
不 retained,没有 lifecycle。这点跟 SwiftUI / Compose 不同(它们
都 retained + diff);跟 iced / egui 一致。

**命名**取 SwiftUI 的 `View`(不是 Flutter 的 `Widget`,也不是
CSS 的 `Element`)——
- `View` 跨语义:既能是叶子也能是容器
- SwiftUI 是当前桌面 / iOS UI 的事实标准,术语普及度最高
- 我们继承命名 → 学习曲线 = 0

### 5.2 Modifier chain

API 形态走 SwiftUI / Compose fluent chain:

```rust
Text::new("hello")
    .color(token::color::accent)
    .size(TextSize::Header)
    .padding(token::space::MD)
    .background(token::color::bg_raised)
    .border(Length::Pt(1.0), token::color::border)
    .corner_radius(token::radius::MD)
    .frame(width = Length::Pct(1.0))
```

**绝对不写**:

```rust
// ❌ wrapper struct 嵌套 — 2010-era 形态
Sized {
    width: Some(Length::Pct(1.0)),
    child: Box::new(Pad {
        padding: Edges::all(12.0),
        child: Box::new(Bordered {
            ...
            child: Box::new(Text { ... })
        })
    })
}
```

Modifier chain 内部还是返 `View::Modified(...)`,但 API 用户不直
接见。这是 SwiftUI / Compose / Flutter modifiers 走了 8 年验过
的形态,我们直接抄结论。

### 5.3 Modifier 是什么

```rust
pub enum Modifier {
    Padding(Edges),
    Background(Color),
    Border(Length, Color),
    CornerRadius(Length),
    Shadow(Shadow),
    Frame(FrameSpec),     // width / height / min / max / aspect / alignment
    Offset(Length, Length),
    ZIndex(i32),          // 跟同 ZStack 内同级 view 排 z;不夸 ZStack 边界
    Hidden(bool),
    OnHover(HoverId),     // 注册 hit region 用
    OnClick(ActionId),
    Id(ViewId),           // 稳定 identity,见 §10
}

pub struct Modified {
    pub child: Box<View>,
    pub mods: Vec<Modifier>,   // 顺序敏感:Padding 套 Border 套 BG 跟反过来,效果不同
}
```

应用顺序 = 数组里的顺序。**外侧后加的 modifier 包外侧** —— 跟
SwiftUI / Compose 一致(modifier order matters)。

### 5.4 `Edges` / `FrameSpec` / `Anchor`(明确定义,不再 hand-wavy)

```rust
#[derive(Clone, Copy, Debug)]
pub struct Edges {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}
impl Edges {
    pub fn all(l: Length) -> Self;
    pub fn xy(x: Length, y: Length) -> Self;  // 水平 / 垂直对称
    pub fn only(top: Option<Length>, right: ..., bottom: ..., left: ...) -> Self;
}

#[derive(Clone, Copy, Debug)]
pub struct FrameSpec {
    pub width:  Option<Length>,   // None = hug
    pub height: Option<Length>,
    pub min_w: Option<Length>,
    pub max_w: Option<Length>,
    pub min_h: Option<Length>,
    pub max_h: Option<Length>,
    pub aspect: Option<f64>,      // w / h
    pub align: Anchor,            // 子在自己 frame 里的对齐
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeading,  Top,    TopTrailing,
    Leading,     Center, Trailing,
    BottomLeading, Bottom, BottomTrailing,
}
```

`Anchor` 用 SwiftUI 的 Leading/Trailing(I18N-aware) — 即便我们 v1
不做 RTL,这个命名让未来加 RTL 不破坏接口。Flutter 也是同套
(`AlignmentDirectional`)。

---

## 6. Layout 算法 — Constraints two-pass

抄 Flutter 的算法(也是 Compose / SwiftUI 内部用的),给具体伪码:

### 6.1 数据结构

```rust
#[derive(Clone, Copy)]
pub struct Constraints {
    pub min: (f64, f64),  // (w, h),逻辑 pt
    pub max: (f64, f64),  // 可以是 INFINITY 表示"想多大都行"
}

#[derive(Clone, Copy)]
pub struct Size { pub w: f64, pub h: f64 }
```

### 6.2 算法

```
layout(view, constraints) -> Size:
    match view:
        Text(t):
            text_w = chars * cell_w
            text_h = lines * line_h
            return clamp((text_w, text_h), constraints)

        Filled(_) | Hairline(_):
            return constraints.max  // 吃满给的

        Spacer(s):
            // Spacer 在 Stack 外不应出现;Stack 自己处理
            return constraints.min

        VStack(children, gap, align, distribute):
            // ── Pass A: 量 non-Spacer children 的 intrinsic h ──
            child_sizes = []
            total_intrinsic_h = 0
            n_flex = 0
            sum_flex_weight = 0
            cross_w = 0
            for c in children:
                if c is Spacer(flex):
                    n_flex += 1
                    sum_flex_weight += flex
                    continue
                // 给非 Spacer 子 unbounded h(它能多高就多高)
                c_size = layout(c, Constraints {
                    min: (cross_min, 0),
                    max: (cross_max, INFINITY),
                })
                child_sizes[i] = c_size
                total_intrinsic_h += c_size.h
                cross_w = max(cross_w, c_size.w)

            // ── Pass B: 剩余 height 分给 Spacers ──
            usable_h = constraints.max.h
            gap_total = gap * (children.len() - 1)
            leftover_h = max(0, usable_h - total_intrinsic_h - gap_total)
            // distribute: spaced / between 用 leftover 当 gap;
            // start / center / end 不给 Spacer flex,Spacer 不存在时
            // leftover 直接按 distribute 摆放
            for s in spacers:
                s_h = leftover_h * (s.flex / sum_flex_weight)

            // ── Pass C: 算每个 child 的 y 位置 ──
            ...(按 distribute / align)

            return Size { w: cross_w (或 cross constraint), h: ... }

        HStack: 同 VStack 轴对调

        ZStack(children):
            max_w = max_h = 0
            for c in children:
                c_size = layout(c, constraints)
                max_w = max(max_w, c_size.w)
                max_h = max(max_h, c_size.h)
            return clamp((max_w, max_h), constraints)

        Modified(child, mods):
            // 应用 modifier 链:倒序还原 constraints,然后正序应用 size 包装
            inner_c = constraints
            for m in mods.reverse():
                if m is Padding(e): inner_c = shrink(inner_c, e)
                if m is Frame(f):   inner_c = clamp_to_frame(inner_c, f)
                ...
            inner_size = layout(child, inner_c)
            outer_size = inner_size
            for m in mods:
                if m is Padding(e): outer_size = expand(outer_size, e)
                if m is Frame(f):   outer_size = enforce(outer_size, f, constraints)
                ...
            return outer_size
```

复杂度 = O(N) 节点遍历两遍,跟 Flutter / Compose 同阶。

### 6.3 关键不变量

1. **Constraints 单调** — child 拿到的 max 永远 ≤ parent 拿到的 max
   减去同级开销(gap / padding)
2. **child 永远满足 parent 给的 constraints** — clamp 在 child 自己
   的 layout 里完成;parent 信任 child 返回的 size 已 clamped
3. **Pct 在 unbounded 轴上 = 0**(没法 100% 一个 infinity)— 调用者
   需要显式 frame 才能让 Pct 工作

---

## 7. Layout primitives

| Primitive | 等价 |
|---|---|
| `VStack { gap, align, distribute }` | CSS `flex-direction: column` |
| `HStack { gap, align, distribute }` | CSS `flex-direction: row` |
| `ZStack { align }` | CSS `position: absolute` 叠 |
| `Spacer { flex }` | CSS `flex: N` |
| `Text` modifier `.frame()` | CSS `width / height` |
| `Text` modifier `.padding()` | CSS `padding` |

### 7.1 没有这些

- **CSS Grid 完整规范** — chrome 用例少;Grid 类布局用 nested
  V/HStack 替代,丢一点表达力换实现简单。需要时再加。
- **Flexbox 完整** — 我们只取 row/column + spacer + gap + alignment
  + distribute。`flex-wrap` / `order` / `flex-basis vs flex-grow vs
  flex-shrink` 三件套全砍。
- **Float / 文字环绕** — 不做。
- **绝对定位 `position: absolute`** — 用 ZStack 替代;我们没 fixed /
  sticky 概念。

---

## 8. Hit Testing

### 8.1 设计

View 树跑完 layout 后,**每个 view 知道自己的 rect**。Hit testing
= 从 root 往下走,后序遍历找最深的命中:

```rust
fn hit(laid_out: &LaidOutTree, p: (f64, f64)) -> Option<HitTarget> {
    // 后序 = 子 → 父,后画的(submission order 在上)优先命中
    for child in laid_out.children.iter().rev() {
        if let Some(h) = hit(child, p) { return Some(h); }
    }
    if laid_out.rect.contains(p) {
        if let Some(action) = laid_out.on_click { return Some(action); }
    }
    None
}
```

`OnClick(action_id)` 是个 modifier:

```rust
HStack { ... }
    .padding(token::space::MD)
    .on_click(ActionId::ToolbarSidebar)
```

`ActionId` 是项目枚举(`marspot::ActionId`),不是 closure。事件
循环捕到 click → hit_test → action_id → 路由到 reducer。这是 elm
/ redux 套路 —— **完全可序列化**(testable / debuggable),好过
closure-based。

### 8.2 跟 SwiftUI / Compose / iced 对比

| | Closure-based(SwiftUI/Compose/iced) | ActionId-based(marspot) |
|---|---|---|
| 写法 | `.onTapGesture { state.toggle() }` | `.on_click(ActionId::ToggleSidebar)` |
| state 来源 | closure capture | reducer 拿 ActionId 改 state |
| testable | closure 难 unit-test | reducer 纯函数 |
| serializable | 否 | 是(可以日志 / 回放) |
| 写法噪声 | 短 | 长一行(需要 enum 定义) |

closure-based 是更"看起来熟"的现代风。**marspot 反其道,选 ActionId**
理由:
- marspot 已经走 elm-ish 架构(`CoreEvent` enum → main loop dispatch)
- closure 在 immediate-mode 里管 lifetime 痛(每帧重建 closure 还
  要持有什么?)
- 日志 / 回放 / debug 完整可序列化的事件链 = 长期债务大幅降低

---

## 9. State + Interaction

### 9.1 单向数据流

```
        host state (DevPanelState / ContextMenuState / ...)
              │
              ▼
        build_view(state) -> View tree
              │
              ▼
        layout(View, Constraints) -> LaidOut tree
              │
              ├─→ paint → Canvas → encode_canvas → GPU
              │
              └─→ hit_test(LaidOut, pointer) -> Option<ActionId>
                              │
                              ▼
                       reducer(state, ActionId) (改 host state)
                              │
                              ▼
                  request_redraw() → 回到 build_view
```

跟 elm / redux 完全一致。**没有响应式信号(SolidJS / Leptos),没有
virtual DOM(React),没有双向绑定(Vue)。**

### 9.2 Hover / Focus

- **Hover** = host 持 `hover_id: Option<HoverId>`,pointer move 命中
  hit_test 改 hover_id,redraw。`.on_hover(HoverId)` modifier 注册
  hit region。
- **Focus** = host 持 `focus_id: Option<FocusId>`,Tab / Shift+Tab
  在 focusable 环里走。`.focusable(FocusId)` modifier 加入环。

跟 click 同 ActionId 模式,只是 enum 改名。

---

## 10. Identity / Keys(动画铺垫)

**问题**:无 retained tree 时,怎么让 "同一个" view 跨帧保持身份?
两种场景需要:
- **未来动画**:某 row 从 y=20 → y=40 平移,需要知道这是同一个 row
- **现在文本输入框**:光标 / 选区状态附着在某个 view 上,view 重
  build 之后状态不能丢

**SwiftUI / Compose 解法**:用 view 在树里的**结构位置**作隐式
identity(`ForEach` 提供显式 id 时用显式)。Flutter 用 `Key`。

**marspot 解法**:`Modifier::Id(ViewId)` —— 显式 view id。host 持
一个 `Map<ViewId, ViewState>`,每帧 build 时不变;view 内部状态
通过这个 map 访问。

v1 暂不需要 stateful view(我们只有 chrome,所有 state 都在
host),但 modifier 留 slot,未来加 input field 直接用。

---

## 11. Typography

### 11.1 v1

- 单一 monospace chrome font(跟 terminal grid 用的字体)
- `TextSize` enum:`Caption(0.85)` / `Body(1.0)` / `Header(1.2)` /
  `LargeHeader(1.5)`,各乘 cell_h 得 line height
- `.weight(Regular | Bold | Dim)` — Dim = alpha 模糊化
- `.align(Leading | Center | Trailing)`
- `.lines(Single { truncate: End | Middle | None } | Wrap { max: u32 })`

### 11.2 不做

- 字符级 span / rich text(terminal grid 自己 cell-level 着色,
  chrome 用不上)
- 多 font-family(增加 font cache / glyph atlas 复杂度,marspot
  chrome 不需要)
- 字间距 / 行间距单独可调(跟 size 绑死即可)

参照:CSS `font-size` / SwiftUI `Font.Style` / Compose `Typography`
都比这复杂一个量级;我们故意收窄。

---

## 12. Theme

v1 = 一个 `Dark` theme 跑天下。Token module 是单一 source of
truth,组件代码引用 token 名,不引用字面值。

**架构上预留**(不实施):

```rust
pub enum ThemeId { Dark, Light, HighContrast, /* future user themes */ }
pub fn theme() -> &'static Theme  // 全局
pub fn set_theme(id: ThemeId)
```

切 theme = 改全局 + 全 redraw。简单粗暴,够用。

---

## 13. 渲染管线

```
host state
   │
   ▼
[build_view(state)]    ← 每帧调,纯计算,返 View tree
   │
   ▼ View tree
[layout(view, root_constraints)]    ← Constraints two-pass
   │
   ▼ LaidOut tree (每节点有 rect)
[paint(laid_out)]    ← 后序 traversal,push 进 Canvas
   │
   ▼ Canvas (Primitive queue)
[encode_canvas(canvas, encoder)]    ← 已落,GPU 提交
   │
   ▼ Metal pipeline
   pixels
```

**每帧重建整树,不复用**。代价:O(N) build + O(N) layout + O(N)
paint。在 marspot chrome 规模(几十到几百节点)下完全免费(<0.5ms)。
省下来不要 retained tree / diff / reconciliation 的复杂度,**净
赚**。

---

## 14. Animation(v2+)

**v1 完全不做**。理由:
- marspot 核心 perf 红线 = `idle CPU = 0`。Animation 要么持续
  redraw,要么事件驱动 schedule 下一帧 —— 都跟 idle 0 冲突
- 没有真用户报"动画不够顺滑"
- chrome UI 用静态切换体验已经够

**v2+ 路径**(留架构空间):

```rust
pub struct Anim<T> {
    pub from: T,
    pub to: T,
    pub started_at: Instant,
    pub duration: Duration,
    pub curve: Curve,
}
```

redraw 时 host 检查活跃 Anim,有 → schedule 下一帧 → 按 t 插值;
无 → 0 redraw。**不引入 retained tree**,依然是 immediate mode +
插值 state。

参照:SwiftUI `withAnimation` 是这模型;CSS transitions 也是
(只是声明在 stylesheet)。

---

## 15. Accessibility / i18n(v2+)

- v1 不做 VoiceOver / NSAccessibility tree
- v1 不做 RTL
- 必须留接口:每个 actionable view 至少有 `ViewId`,将来 AX label
  能挂上去

---

## 16. 实施 roadmap

按 effort × 收益:

| 阶段 | 内容 | LOC | 阻塞 |
|---|---|---|---|
| **P3a** | `token` module 收口,各 component 改用 token | ~200 | — |
| **P3b** | `Length::Ch(N)` + Canvas builder 支持 | ~80 | — |
| **P3c** | `View` enum + `Modified` + Modifier 链 + `Edges` / `FrameSpec` / `Anchor` 类型 | ~250 | P3a |
| **P3d** | `Constraints` + `layout(view, c)` two-pass | ~400 | P3c |
| **P3e** | `VStack` / `HStack` / `Spacer` 实现 | ~250 | P3d |
| **P3f** | `ZStack` 实现 | ~100 | P3e |
| **P3g** | Text 上 `.size` / `.weight` / `.align` / `.truncate` | ~200 | P3c |
| **P3h** | Hit testing tree-walk + `ActionId` 路由 | ~150 | P3e |
| **P3i** | Migrate ContextMenu / LayoutModal / DevPanel / Table / Sidebar 上 View 树 | ~600 | P3g + P3h |
| **P3j** | `ViewPainter` 退役 | ~−400(净删) | P3i |
| **P3k** | `theme()` global + ThemeId(Dark only impl)| ~120 | P3a |

总:~2150 LOC 加,~400 删,**净 +1750 LOC**。可分 1-2 周完成,
P3a → P3k 串行 / 部分可并行。

---

## 17. 文件结构(实施后)

```
src/ui/
├── core/                       ← L1 + L3
│   ├── units.rs                Length (Pt / Pct / Ch)
│   ├── color.rs                Color
│   ├── canvas.rs               Canvas (lower-level than View, paint backend)
│   └── mod.rs
├── theme/                      ← L1 token (P3a / P3k)
│   ├── token.rs                color / space / radius / typography
│   ├── dark.rs                 Dark theme 实例
│   └── mod.rs
├── view/                       ← L4 (P3c-h)
│   ├── view.rs                 enum View + trait helpers
│   ├── modifier.rs             Modifier enum + Edges / FrameSpec / Anchor
│   ├── stack.rs                VStack / HStack / ZStack
│   ├── spacer.rs               Spacer
│   ├── text.rs                 Text view + 字号 / weight / align / truncate
│   ├── constraints.rs          Constraints + layout pass
│   ├── paint.rs                paint pass (LaidOut tree → Canvas)
│   ├── hit_test.rs             Hit testing
│   └── mod.rs
├── components/                 ← L5 (P3i — 整体重写)
│   ├── dev_panel.rs
│   ├── context_menu.rs
│   ├── layout_modal.rs
│   ├── table.rs
│   ├── sidebar.rs
│   └── mod.rs
└── system/macos/               ← AppKit 桥(不在本 doc 范围)
```

---

## 18. State of the Art Check — 这是最先进的模型吗?

**Yes, for our scope**。逐家对比:

### vs SwiftUI(2019-now)

| 维度 | SwiftUI | marspot | 差异 |
|---|---|---|---|
| API 形态 | View struct + modifier chain | View enum + modifier chain | **持平** |
| 树形 | retained + diff | immediate(每帧重建) | 取舍:我们简单,SwiftUI 自动状态延续 |
| 状态 | `@State` / `@Binding` property wrapper | host state + ActionId reducer | 取舍:我们 testable + serializable,SwiftUI 写法短 |
| 动画 | `withAnimation` 自动插值 | **v1 没有** | SwiftUI 赢,但 v2+ 我们能补 |
| Layout | Constraint propagation | Constraints two-pass | **持平** |
| AX | 自动 + 手动 hint | **v1 没有** | SwiftUI 赢,但接口 reservation 留好 |

**结论:核心 API 持平,生态 / 自动化少一截 —— 但这是 marspot
scope 的合理取舍,不是技术债**。

### vs Jetpack Compose(2021-now)

跟 SwiftUI 一样的模型(retained + modifier chain),实现细节差。
Compose 用 Kotlin coroutines / Recomposer 做 incremental update,
marspot 不需要(immediate rebuild 已够)。**持平**。

### vs Flutter(2018-now)

- Layout 算法:**完全一致**(Constraints two-pass)
- Widget tree:Flutter retained,marspot immediate — 同上取舍
- Rendering:Flutter Skia,marspot Metal + Canvas — 持平
- State:Flutter `InheritedWidget` / `Provider`,marspot host state —
  marspot 简单

### vs iced(Rust)

- API:iced fluent + closure-based 事件,marspot fluent + ActionId
  enum — marspot 更 redux-y / serializable
- Layout:iced 内部也 constraints,但封装不如 Compose 干净 — **持平**
- 渲染:iced wgpu 多后端,marspot Metal 单后端 — 跨平台 iced 赢
  但 marspot 不要跨平台

### vs egui(Rust immediate-mode)

- API:egui 还是 imperative `ui.button("hi")` 风;marspot declarative
  View 树 — **marspot 更现代**
- Layout:egui 顺序流式简陋;marspot constraints 算法完整 —
  **marspot 赢**
- 状态:egui 内部状态藏 widget id 里,marspot host state 显式 —
  **marspot 干净**

### vs CSS + DOM(浏览器栈)

不在同一量级。浏览器栈是 30 年技术债 + ergonomic 沼泽。marspot
取 CSS 的**心智模型**(box-sizing / px / % / rgba),砍掉**实现复
杂度**(cascading / specificity / pseudo-classes / @media / flex-wrap
spec 等)。**这是现代 UI 框架的共识**(SwiftUI / Compose / Flutter
都这么做)。

---

## 19. 明确不做(避免范围蠕变)

| 不做 | 原因 |
|---|---|
| 完整 CSS Grid | nested Stack 够 |
| `flex-wrap` / `order` / `flex-basis` | 增加 layout pass 复杂度,chrome 用例少 |
| margin(包括 negative margin) | margin collapse 是公认设计错误,SwiftUI/Compose/Flutter 都没 |
| `position: sticky / fixed` | 终端 chrome 没用 |
| CSS animation / transition / keyframes | v1 不做,v2+ 走 Anim<T> 路径 |
| CSS transform / matrix3d | 没场景 |
| Pseudo-classes(`:hover` / `:focus`)| 走 host state + modifier(`.on_hover`) |
| Media queries / 响应式 | 只有 dev panel + 主窗 两个 NSWindow,直接 Pct |
| 响应式 / signals / observable | immediate-mode 够 |
| Virtual DOM / diff / reconciliation | 每帧重建,N 小,免费 |
| 多 font-family / icon font | SDF + glyph atlas 已经覆盖 |
| 多 theme JSON / TOML | 自用阶段没人配 |
| 多语言 / RTL / unicode bidi | v1 中英文 LTR 够 |
| Subpixel font hinting | macOS 已默认走 grayscale,我们跟 |
| Print stylesheet | (笑)|

---

## 20. 一句话

**marspot UI v1 = CSS 心智 + SwiftUI 命名 + Flutter Constraints
two-pass + immediate-mode 渲染 + elm/redux 状态流。** 砍掉
reactivity / 完整 CSS / animation / 多 theme / i18n / AX,留下
View tree + Modifier chain + token system 三件套,做 chrome 类
UI 长期最简最稳的形态。

**Is this the most advanced model for our scope?** 是。跟 SwiftUI /
Compose / Flutter 的 layout 算法持平,跟它们的 retained tree 取
舍我们选 immediate(代价:无自动 state 延续 / 无自动动画 — 用
ActionId + Anim<T> v2+ 补)。比 iced / egui 的 API 形态更现代。
比 CSS / DOM 的实现复杂度低两个数量级 —— 这是 SOTA 的共识方向,
我们站在这条线的最右侧。
