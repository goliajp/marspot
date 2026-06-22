# marspot UI 模型 — 完整设计

> 2026-06-23 起草。把现有 `Length` / `Color` / `Canvas` 三件套
> 扩成一个**完整可落地的 UI 系统**,够 marspot chrome(title strip /
> sidebar / modal / dev panel / tab strip 一类)长期用,不奢望做
> 通用 GUI 框架。
>
> 参照系:CSS / SwiftUI / Flutter / iced。每条都标注我们抄了谁、
> 砍了谁、为什么。

---

## 0. 设计原则(总纲)

1. **够 chrome 用就停**。marspot 不是浏览器,不需要完整 CSS。
   不做的事比做的事更重要 —— 见 §13。
2. **CSS 类术语优先**。`Pt(1.0) = 1px`、`Pct(0.5) = 50%`、`rgba(...)`、
   `border-radius`、`box-shadow`,熟悉的人零学习曲线。
3. **声明式 + 立即模式**。Canvas 已经是 immediate-mode,新加的 layout
   primitives 继续往这条路走,不引入 retained tree / 双向数据流。
4. **submission order = z order**(已建立的不变量,继续保持)。
5. **一种长度类型** = `Length`。位置 / 尺寸 / 间距 / radius 全用它,
   不区分 width / height 单位族。
6. **不发明新名词**。VStack 就叫 VStack,padding 就叫 padding。

---

## 1. 概览 — 五层

```
┌──────────────────────────────────────────────────┐
│  L5  Components   ContextMenu, Tabs, Modal, ...  │
├──────────────────────────────────────────────────┤
│  L4  Layout       VStack, HStack, ZStack,        │
│                   Spacer, Pad, Sized             │
├──────────────────────────────────────────────────┤
│  L3  Primitives   rect / line / text  (Canvas)   │
├──────────────────────────────────────────────────┤
│  L2  Box model    fill / border / radius /       │
│                   shadow / padding / margin      │
├──────────────────────────────────────────────────┤
│  L1  Foundation   Length / Color / Tokens        │
└──────────────────────────────────────────────────┘
```

**L1-L3 已落地**。**L4-L5 是这份设计的新区**。
ContextMenu / LayoutModal 已部分迁到 Canvas(L5 over L3),
但走的是各自手算坐标,没经过 L4 layout —— 这是要回填的。

---

## 2. L1 Foundation

### 2.1 `Length`(已落)

```rust
pub enum Length { Pt(f64), Pct(f64) }
```

| | CSS | SwiftUI | Flutter | marspot |
|---|---|---|---|---|
| 绝对 | `px` | `Length` fixed | `LogicalPixels` | `Pt(N)` |
| 相对 | `%` of parent | `Length::flexible` | `FractionallySizedBox` | `Pct(F)` |
| viewport | `vw/vh` | n/a(GeometryReader)| `MediaQuery` | **不做**(只有窗口,直接 Pct 拿) |
| em | font-relative | `@ScaledMetric` | `MediaQuery.textScaleFactor` | **不做**(单 font cell) |
| 字符宽 | n/a | n/a | n/a | **加**:`Ch(N)`(N 个 chrome cell 宽)|
| auto | `auto` | hug content | intrinsic | **加**:`Auto`(hug 内容,见 §5.2)|

**新增** `Length::Ch(f64)` —— marspot 是终端,经常按"几个字符宽"算位置
(如 menu padding 12pt 其实是 1.5 个 cell);专门的 cell-unit 比每次
`Pt(cell_w * n)` 干净。

**新增** `Length::Auto` —— 让节点自己决定尺寸(对 text / 内容 hug 的
容器有意义)。CSS 默认就是 auto;Canvas 当前模型显式给 size 反而是
特例。

### 2.2 `Color`(已落)

```rust
pub struct Color { r: u8, g: u8, b: u8, a: f32 }
impl Color { pub const fn rgba(r: u8, g: u8, b: u8, a: f64) -> Color }
```

跟 CSS `rgba(...)` 一一对应。**不加**:HSL / OKLCH / color-mix /
named-color 表 —— 终端 chrome 用不上那么精细的色彩空间,需要时再加。

### 2.3 Tokens — 语义色 + 间距尺度(新)

跟 CSS 变量 + Tailwind / Apple HIG 类似:

```rust
pub mod token {
    pub mod color {
        pub const fg:         Color = Color::rgba(232, 238, 248, 1.0);
        pub const fg_muted:   Color = Color::rgba(140, 153, 168, 1.0);
        pub const bg:         Color = Color::rgba(20,  22,  28,  1.0);
        pub const bg_raised:  Color = Color::rgba(28,  31,  39,  1.0);
        pub const bg_selected:Color = Color::rgba(51, 107, 173, 1.0);
        pub const border:     Color = Color::rgba(56,  60,  70,  1.0);
        pub const accent:     Color = Color::rgba(91, 162, 250, 1.0);
        pub const danger:     Color = Color::rgba(220, 60,  60,  1.0);
        // ...
    }
    pub mod space {
        pub const XS: Pt = Pt(4.0);
        pub const SM: Pt = Pt(8.0);
        pub const MD: Pt = Pt(12.0);
        pub const LG: Pt = Pt(16.0);
        pub const XL: Pt = Pt(24.0);
    }
    pub mod radius {
        pub const SM: Pt = Pt(3.0);
        pub const MD: Pt = Pt(6.0);
        pub const LG: Pt = Pt(10.0);
    }
}
```

**规则**:组件代码用 `token::color::accent`,**不用** `Color::rgba(91, 162, 250, 1.0)`。
将来切 light theme / Solarized / 任意配色,改 token 模块一个地方。
现状是 token 散在各组件里(dev_panel 自己 mod tokens,context_menu
里也是字面色),要收口到这个中心 module。

参照系:
- CSS:Custom Properties (`--accent: ...`)
- SwiftUI:`Color.accentColor` / `.semantic` (`.primary`, `.secondary`)
- Tailwind:`text-blue-500` / `space-y-4`
- Material:`md.sys.color.primary`

---

## 3. L2 Box Model — 补齐 padding / margin

### 3.1 现状

`Rect` builder 已经有:
- `.at(x, y)` — 绝对定位(top/left)
- `.size(w, h)` — 显式 width/height
- `.fill(color)` — background-color
- `.border(width, color)` — 1pt inside-stroke(border-box semantics)
- `.radius(r)` — border-radius
- `.shadow(blur, off, color)` — box-shadow

**缺**:padding(内填充,推 content 内缩),margin(外间距,推同级
元素远离)。Canvas 当前没有"子元素"概念,所以 padding 无处生效;
margin 同理。**这些是 layout 系统(L4)的事,不在 Rect 上**。

### 3.2 决议:box-sizing 永远 border-box

```
┌─ margin ─────────────────────────────────┐
│  ┌─ border ─────────────────────────┐    │
│  │  ┌─ padding ──────────────────┐  │    │
│  │  │      content (w × h)        │  │    │
│  │  │                              │  │    │
│  │  └─────────────────────────────┘  │    │
│  └───────────────────────────────────┘    │
└───────────────────────────────────────────┘
        ↑ size(w, h) 量的是这一层(border 外缘)
```

跟 CSS `box-sizing: border-box` 一致 —— 写 `size(100, 100)` 就是
**总占地 100×100**,不管 border 有多粗。SwiftUI / Flutter / Tailwind
默认都这样。**不支持 content-box**(原始 CSS 默认),太容易踩坑。

### 3.3 inside-stroke 已定

border 内侧描边(已实现):写 `border: 2px solid` 不会让框比 size
大 2px,而是从内侧吃掉 2px。跟现代 web 工具链 / SwiftUI / Flutter
一致。

---

## 4. L3 Primitives — Canvas

已落,见 `docs/ui-system-rfc.md` 跟 `dev panel/UI tab/Model` section。
不在这份 doc 重复。

待加:
- **Image** primitive(PNG / 二进制 raw bytes / IOSurface ref)— 现在没
  必要(chrome 都是矢量),先不做,留 enum slot。
- **Path** primitive(SVG-like beziers)— 同上,需要时再加。

---

## 5. L4 Layout — 新增

CSS / SwiftUI / Flutter 三家对照:

| 概念 | CSS | SwiftUI | Flutter | marspot 取舍 |
|---|---|---|---|---|
| 垂直栈 | `flex-direction: column` | `VStack` | `Column` | **`VStack`** |
| 水平栈 | `flex-direction: row` | `HStack` | `Row` | **`HStack`** |
| 叠层 | `position: absolute` | `ZStack` | `Stack` | **`ZStack`** |
| 间距 | `gap` | `spacing` | `mainAxisSpacing` | **`gap: Length`** |
| 交叉轴对齐 | `align-items` | `alignment` | `crossAxisAlignment` | **`align`**(`start`/`center`/`end`/`stretch`)|
| 主轴分布 | `justify-content` | n/a(用 Spacer)| `mainAxisAlignment` | **`distribute`**(`start`/`center`/`end`/`spaced`/`between`) |
| flex-grow | `flex: N` | `Spacer()` | `Expanded(flex: N)` | **`Spacer`** + `.flex(N)` |
| padding | `padding` | `.padding()` | `Padding` | **`Pad`** wrapper |
| 显式 size | `width/height` | `.frame()` | `SizedBox` | **`Sized`** wrapper |
| min/max | `min-width / max-width` | `.frame(minWidth:)` | `ConstrainedBox` | **`.min(L)` / `.max(L)`** on Sized |
| 网格 | CSS Grid | `Grid`(iOS16+)| `GridView` | **暂不做**(用嵌套 Stack 替代;chrome 用例少)|
| 流式 wrap | `flex-wrap: wrap` | `WrapHStack`(iOS17+)| `Wrap` | **暂不做** |

### 5.1 核心五个 layout primitive

```rust
// 垂直栈 - main axis 是 y,cross axis 是 x
pub struct VStack {
    pub children: Vec<Element>,
    pub gap: Length,            // 默认 0
    pub align: AlignCross,      // 默认 Start
    pub distribute: Distribute, // 默认 Start
}

// 水平栈 - main axis 是 x
pub struct HStack { /* 同 VStack,轴对调 */ }

// 叠层 - 子元素堆叠,各自带 anchor / offset
pub struct ZStack {
    pub children: Vec<(Anchor, Element)>,
}

// 包一层 padding
pub struct Pad {
    pub child: Box<Element>,
    pub padding: Edges,   // top / right / bottom / left
}

// 显式 size / min / max / aspect
pub struct Sized {
    pub child: Box<Element>,
    pub width:  Option<Length>,
    pub height: Option<Length>,
    pub min: Edges, // (w_min, h_min)
    pub max: Edges, // (w_max, h_max)
    pub aspect: Option<f64>, // w/h
}

// Flex spacer — 在 VStack/HStack 里吃剩余空间
pub struct Spacer { pub flex: u32 }  // 默认 1
```

### 5.2 Sizing 规则(关键)

跟 Flutter / SwiftUI 一致的两遍 layout:

**Pass 1(down)** — 父给 child 一对 `Constraints`(`min/max` per axis)。  
**Pass 2(up)** — child 在那对 constraints 里挑自己的 actual size,返回给父。

```rust
pub struct Constraints {
    pub min_w: f64, pub max_w: f64,
    pub min_h: f64, pub max_h: f64,
}
pub trait Layout {
    fn layout(&self, c: Constraints) -> Size;
}
```

每种节点的 sizing 行为:

| 节点 | layout 行为 |
|---|---|
| `Text("...")` | size = (`width = chars * cell_w` clamped to constraints, `height = line_h * line_count`)|
| `Rect.size(w, h)` | size = `(resolve(w, parent_w), resolve(h, parent_h))` clamped |
| `VStack` | 主轴:children 各占自己想要的高度 + gap;`Spacer` 分剩余。交叉轴:Pct(1) 子元素拉到 parent.w |
| `HStack` | 轴对调同上 |
| `ZStack` | size = 最大子元素;子元素拿 parent 的 max constraints |
| `Pad(c, padding)` | child 拿 `parent - padding` 作 max;Pad self = child.size + padding |
| `Sized.width=Some(L)` | child 拿 `(L, L)`(强制);Sized 自身 = (L, child.h) |
| `Sized.aspect=Some(r)` | 在 constraints 里挑最大可行的 `(w, h)` 满足 `w/h == r` |

**Hug** = "不设 size 让 layout 自己定" = `width: None`,跟 SwiftUI 的
"hug content" 一致。Flutter 没这词,但 `mainAxisSize: min` 是同意思。

**Fill** = `width: Some(Pct(1.0))`,跟 CSS `width: 100%` 同义。

### 5.3 Distribute / Align(主轴 + 交叉轴)

```
distribute(主轴):
  start    [A B C            ]
  center   [    A B C        ]
  end      [          A B C  ]
  spaced   [A    B    C       ]  (gap between + before + after)
  between  [A     B     C    ]  (gap between only)

align(交叉轴):
  start    [A|B|C]  → 顶/左对齐
  center   [A B C]  → 居中
  end      [A|B|C]  → 底/右对齐
  stretch  child 拉到 parent 交叉轴满
```

跟 CSS `justify-content` / `align-items` 一对一,只是 `spaced` =
`space-around`,`between` = `space-between`,名字短一点。

---

## 6. State + Interaction(轻量化)

### 6.1 现状

各 component 自己拥有状态(`DevPanelState` / `LayoutModalState` /
`ContextMenuState`),L1/L2 持有,渲染时传给 build_*_canvas;
click 命中由 `hit_test(...) -> Option<Hit>` 给 caller 自己分发。

### 6.2 不引入

- **响应式 / signals / observables**(SolidJS / Leptos 风格)— immediate
  mode 已经够,加 reactivity 是 reset framework
- **virtual DOM diff / reconciliation**(React) — 每帧重建 Canvas,
  diff 没意义
- **bind = bind**(SwiftUI `@Binding`) — 状态在 App 自己手里,直接
  改 + redraw 就行

### 6.3 加这些

- **hover state**:`hover_id: Option<HoverId>` 在 host state 里;
  pointer move 事件命中后改 hover_id,redraw 时 component 按
  `hover_id == self.id` 选不同 token color
- **focus chain**:Tab / Shift+Tab 走环形 focus 序列。每个 focusable
  element 拿个 `focus_id`,host 持 `focus: Option<FocusId>`
- **click action**:延续现在的 `hit_test → Hit enum → caller match` 
  模式,**不**升级到 closure-based handler(closure 在 immediate-mode
  里要管生命周期,得不偿失)

参照系:
- iced / egui:closure-based handler — **不要**(state 漏到 closure
  里很难追)
- SwiftUI `.onTapGesture { ... }` — closure-based,同上
- 我们这套接近 **redux dispatch**:UI 出 Action,reducer 吃 Action
  改 state

---

## 7. Typography

### 7.1 暂只一种字体

marspot chrome 用单一 monospace font(同 terminal grid 用的字体)。
**不做** font-family / weight / italic 切换。理由:chrome 类用 mono
看起来稳定一致,正文用 mono 也读得动,加 sans-serif 会破坏视觉
一致性。需要的话以后单加(很可能不需要)。

### 7.2 加这些

- **`.size(Size)`** — `Size` enum:`Body` / `Caption` / `Header` /
  `LargeHeader`,各 map 到 cell_h 的固定倍数(1.0 / 0.85 / 1.2 / 1.5)
- **`.weight(Weight)`** — `Regular` / `Bold` / `Dim`(用 alpha 模拟,
  mono 字体一般没真 bold 字面)
- **`.align(TextAlign)`** — `Leading` / `Center` / `Trailing`
- **`.lines(LinesMode)`** — `Single { truncate: Truncate }` / `Wrap { max: u32 }`
- **`.truncate(Truncate)`** — `End`(末尾加 `…`)/ `Middle`(中间)/ `None`(裁掉)

CSS 对照:`font-size` / `font-weight` / `text-align` / `white-space` /
`text-overflow`,只是收窄了选择。

### 7.3 不做

- 字符级 styling(span / mark / rich text)— terminal 内 grid 已经
  cell-by-cell 着色,chrome 用不上
- 行间距 `line-height` 单独可调 — 跟 size 绑定即可
- 字间距 `letter-spacing` — mono 字体没意义

---

## 8. Theme

### 8.1 现状

只 dark theme(marspot 的固定调性)。各 component 私藏 mod tokens
跟字面 rgba 混用。

### 8.2 目标

- **一个全局 `Theme`**,所有 token 走它取
- **`ThemeId::Dark` / `ThemeId::Light` / `ThemeId::HighContrast`** —
  v1 只实现 Dark
- 切 theme = 改一个 ID 全 redraw

### 8.3 不做

- 用户自定义 theme JSON / TOML — 等真有用户问再加(自用阶段 fork
  改 token 即可)
- per-component override — 走 token 名,组件代码不该跟字面色绑定
- 动态 color-mix / 计算色 — 用 alpha + bg blend 已经够

---

## 9. Animation

### 9.1 v1 不做

所有状态切换都 snap(立即 redraw)。理由:
- chrome 跟 terminal 的关键 perf budget 在 `idle CPU = 0`;animation
  要么 60fps redraw(violates idle 0),要么挑事件驱动重画(复杂度
  陡增,bug 源大头)
- marspot 还没有 "需要 animation" 的真实用户报错;先把 layout 落
  稳再说

### 9.2 v2+ 路径(留架构空间)

走 **time-based interpolation**(类似 SwiftUI `withAnimation`):
- `Anim<T>` 表示某状态 from → to 在 duration 时间窗内的 t curve
- redraw 时 host 检查有没有活跃 Anim,有就在下一帧 schedule redraw
  并按 t 插值;无就 0 redraw(idle 不动)
- 不引入 retained scene graph;依然是 immediate mode + 插值的 state

---

## 10. 渲染管线

### 10.1 现状(已落)

- 每帧 component 各自 `build_*_canvas(state, ...) -> Canvas`,提交
  Primitive 队列
- `encode_canvas` 把队列按 submission order 切成 runs(相邻同
  pipeline 合并),per-run 一个 render encoder
- submission order = z order,无 z-index

### 10.2 加这一层(L4 layout 落地后)

- **layout 阶段**:`Element` tree 走 `layout(Constraints)` 两遍跑,
  产 `LaidOut { rect, child_rects }` 树
- **paint 阶段**:tree 后序遍历,每节点把自己的 primitives push
  进 Canvas
- 两阶段加起来 = 单帧"build Canvas"的内部细分,对外 API 不变

参照:Flutter 渲染管线(layout → paint → composite);marspot
合并了 paint + composite,跳过 retained scene。

---

## 11. Accessibility / i18n

### 11.1 v1 不做

- VoiceOver / AX tree — 自用阶段没意义,真要交付别人时再加,
  AppKit `NSAccessibility` 有现成 protocol 可接
- RTL — marspot 主要中文 + 英文,都 LTR
- 高对比模式 — 留 `ThemeId::HighContrast` 槽位

### 11.2 必须留接口

- **每个可点击 element 至少有个 `id`** 字符串(用于将来 AX label)
- text 已经是 utf-8 string,改 UI 文案不动渲染

---

## 12. 实施 roadmap

按 effort × 收益:

| 阶段 | 内容 | LOC | 阻塞 |
|---|---|---|---|
| **P3a** (next) | tokens 模块收口,所有 component 改用 token 名 | ~200 | 无 |
| **P3b** | `Length::Auto` + `Length::Ch(N)`,Canvas builder 支持 | ~80 | 无 |
| **P3c** | `Pad` / `Sized` wrapper(box model 完整化)| ~150 | tokens 落 |
| **P3d** | `VStack` / `HStack` / `Spacer` + Constraints 算法 | ~400 | Pad/Sized 落 |
| **P3e** | `ZStack`(简化 ContextMenu / Modal 内部代码) | ~150 | VStack 落 |
| **P3f** | Text `.size` / `.weight` / `.align` / `.truncate` | ~200 | Canvas Text builder 扩 |
| **P3g** | Migrate LayoutModal / Table / ProcessPanel / Sidebar 上 layout | ~500 | 上面全落 |
| **P3h** | ViewPainter 退役 | ~-300(净删除) | P3g 完 |

总加起来 ~1700 LOC 新增 + 300 LOC 删除。可分散 1-2 周完成,每个
P3x 自包含可单独 land + install 验。

---

## 13. 明确不做

避免范围蠕变。这些**都不在 marspot UI v1 的边界内**,有需求再
讨论:

- **CSS 完整盒模型变体**(content-box / padding-box / margin auto)
- **CSS Grid 完整规范**(track sizing / span / line names)
- **Flexbox 完整规范**(reverse / wrap / order)
- **Float / inline-block / display:contents**
- **Positioning 完整**(sticky / fixed)— ZStack + anchor 够
- **CSS animation / transition / keyframes / transform**(matrix)
- **Pseudo-classes / -elements**(`:hover` 我们用 host state 模拟,
  不引入伪类语法)
- **Media queries**
- **响应式 / signals / observables**
- **Virtual DOM / diff / reconciliation**
- **CSS variables 完整级联** — token module 一份就够
- **多 font-family / icon font**(我们走 SDF / glyph atlas)
- **i18n / RTL / locale-aware sort / unicode bidi**
- **Print stylesheet**(笑)

---

## 14. 文件结构(实施时长这样)

```
src/ui/
├── core/                  ← L1 + L2 + L3 已经在
│   ├── units.rs           Length (Pt / Pct / Ch / Auto)
│   ├── color.rs           Color
│   ├── canvas.rs          Canvas + Primitive + builders
│   └── mod.rs
├── theme/                 ← 新 (P3a)
│   ├── token.rs           color / space / radius / typography 常量
│   ├── dark.rs            Dark theme 实例
│   └── mod.rs
├── layout/                ← 新 (P3c-e)
│   ├── element.rs         Element enum + trait Layout
│   ├── stack.rs           VStack / HStack / ZStack
│   ├── pad.rs             Pad
│   ├── sized.rs           Sized
│   ├── spacer.rs          Spacer
│   ├── constraints.rs     Constraints / Size
│   └── mod.rs
├── components/            ← L5 (已经在,会持续重写)
│   ├── dev_panel.rs       ← 已用 Canvas;P3g 重写走 layout
│   ├── context_menu.rs    ← 已用 Canvas;P3g 同上
│   ├── layout_modal.rs    ← P3g 重写
│   ├── table.rs           ← P3g 重写
│   ├── sidebar.rs         ← P3g 重写
│   └── mod.rs
└── system/macos/          ← AppKit 桥(独立轴,不归本 doc)
```

---

## 15. 一句话总结

marspot UI = **CSS 心智模型 + SwiftUI 命名 + Flutter constraints
算法 + immediate-mode 渲染**。砍掉 reactivity / 完整 CSS / 多 theme
/ animation,留下最常用的 stack + pad + sized + token 四件套,够
chrome 类 UI 长用,加新组件不发明新 layout 模式。

参照系一句话:

| 系统 | 我们抄了 | 我们没抄 |
|---|---|---|
| CSS | 单位 / box model / rgba / 心智 | 完整 spec / Grid / animation / 选择器 / cascading |
| SwiftUI | VStack/HStack/ZStack 命名 / `.modifier()` builder 风格 / hug-vs-fill | declarative struct 树 / `@State` / property wrapper / Combine |
| Flutter | Constraints two-pass layout / Spacer / Expanded / 单元归一 | retained Widget tree / RenderObject / GestureDetector closure |
| iced / egui | 立即模式精神 | message-based / fluent closure handler / 复杂主题模型 |
