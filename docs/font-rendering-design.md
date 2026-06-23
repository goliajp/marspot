# marspot 字体渲染体系设计(v5)

> 2026-06-23.起因:0.6.27/0.6.28 尝试 SF Pro chrome 渲染撞墙
> ——atlas slot 跟 GlyphInstance size 错位致字符变形,被迫回 Monaco.
> User 要求"专门研究字体渲染,要能正常兼容各种字体,这是 ui 基础能力".
>
> 本 doc 不是浮泛 spec,是把 marspot 现有 atlas/rasterise pipeline 跟
> industry 标准做法逐项对照,提出 v5 设计 + 4 阶段 migration plan.

---

## 目录

- §0 目标
- §1 现状 audit(基于 `src/glyph_atlas.rs` + `src/font_cache.rs`)
- §2 industry 调研:Alacritty / WezTerm / Kitty / iTerm2 / Skia / CoreText / cosmic-text
- §3 设计原则
- §4 glyph cache 键值 v5
- §5 atlas 分配 v5
- §6 raster 流水(rasterization pipeline)
- §7 shaping 流水(text shaping)
- §8 mono 终端 grid 路径(向后兼容)
- §9 sub-pixel 位置(subpixel positioning)
- §10 color emoji
- §11 variable font
- §12 OpenType 特性(kerning / ligature / contextual alt)
- §13 atlas 驱逐(LRU)
- §14 跨平台预备
- §15 4 阶段 migration plan
- §16 待解问题
- §17 性能红线

---

## 0. 目标

1. **同一 atlas 同时服务 mono 终端 + proportional chrome**.不再两条独立 pipeline.
2. **per-glyph slot 真实 bounding box** —— 不再强 stretch glyph 到固定 cell_w × cell_h.
3. **支持任意字体**:mono / proportional / CJK / emoji / variable font / 多 weight.
4. **CoreText 集成** —— 复用 Apple 的 shaping(kerning / ligature / OpenType feature / bidi 自动处理).
5. **mono terminal 严格保持**:cell-grid 对齐 / 整数 baseline / 0 漂移 —— 不能因 chrome 改造破坏 PTY.
6. **subpixel positioning**(可选,8x atlas 代价换 proportional 视觉质量).
7. **LRU 驱逐**,不再无差别 rebuild(每 rebuild = 一帧 stutter).
8. **未来跨平台**(Linux/Windows)backend 替换 path 清晰.

---

## 1. 现状 audit

### 1.1 文件分布

- `src/glyph_atlas.rs` —— GlyphAtlas(shelf packer + FxHashMap cache + R8Unorm/BGRA8Unorm 双 texture format)+ rasteriser
- `src/font_cache.rs` —— FontRegistry(多 font slot 内置 + cascade fallback)+ FontCache(char_cache + style 4 变体 + UI font slot)
- `src/render_metal.rs` —— GlyphInstance + push_text_run / push_text_run_kind / encode_canvas

### 1.2 关键类型(原文)

```rust
// glyph_atlas.rs
pub struct GlyphKey { pub font_id: FontId, pub glyph: CGGlyph }
pub struct AtlasEntry {
    pub u0/v0/u1/v1: u16,   // atlas pixel coords
    pub px_w/px_h: u16,
    pub n_cells: u16,        // 1 or 2 — terminal-grid 概念
}
pub struct SlotMetrics {     // 调用方传入
    pub cell_w/cell_h: u32,
    pub baseline_from_top: u32,
}

// render_metal.rs
pub struct GlyphInstance {
    pub origin: [f32; 2],
    pub size:   [f32; 2],
    pub uv0/uv1: [f32; 2],
    pub color:  [f32; 4],
}
```

### 1.3 rasterise pipeline 现状

```
push_text_run(...)
    ↓
for each char:
    (font_idx, glyph) = font.resolve_char(ch, bold, italic)
    n_cells = char_width(ch)
    atlas.get_or_rasterize(GlyphKey { font_id, glyph }, &ct_font,
                            SlotMetrics { cell_w, cell_h, baseline },
                            n_cells)
        ↓
    cache hit? 直接返
    cache miss → rasterise_glyph(ct_font, glyph, metrics, n_cells)
        ↓
    分配 SlotMetrics{ cell_w × n_cells, cell_h } 大小的 bitmap
    CGBitmapContext + CGContextShowGlyphsAtPositions
    baseline 强制对齐 metrics.baseline_from_top(整数行)
    glyph bbox > slot 时:scale ≤ 1 fit 进去
        ↓
    shelf packer 找位置(高度公差 25%,否则新 shelf)
    upload + insert cache
        ↓
GlyphInstance {
    origin: [x.round(), dest_y],         // x 每 char += cell_w × n_cells
    size:   [cell_w × n_cells, cell_h],  // 强制 cell 大小
    uv: atlas_entry,
}
```

### 1.4 现状的根本约束

**每个 atlas slot = 固定 cell_w × cell_h(单 cell)或 2× cell_w × cell_h(wide)**,不是 glyph 的真实尺寸.结果:

| 场景 | 后果 |
|---|---|
| mono 终端 ASCII | OK —— glyph 本来就该在 cell 里居中 |
| mono 终端 CJK | 大体 OK —— 2-cell slot 容纳得下(只有少数 oversized 走 scale fit) |
| mono 终端 emoji | OK —— bitmap glyph 走 color atlas + scale fit |
| **proportional UI** | **撞墙** —— 所有 glyph 被画进 ui_cell_w 宽的 slot,GlyphInstance 用真实 advance 显示 → 拉伸变形 |

这就是 0.6.27 看到的"字全变形".v5 必须让 atlas slot 跟 glyph 真实 bounding box 一致.

### 1.5 FontCache 现状

- FontRegistry 已支持多 font(by_name 去重)
- char_cache 8K cap,full 时 atomic rebuild
- style 4 变体(regular/bold/italic/bold-italic)
- text-fallback cascade(8 个字体名)
- UI font slot(B0.7 加,目前不被实际调用,但 resolve_char_ui 跟 ui_cell_w/h/ascent 字段已存在)
- **缺**:size 维度 —— FontCache 假定单一固定 size(FONT_POINT=12.0).多 size 渲染(eg chrome 13pt + terminal 12pt)无法表达

---

## 2. industry 调研

### 2.1 Alacritty

- **rasteriser**:`crossfont`(自家 crate,封装 freetype+CT+DirectWrite)
- **atlas**:per-glyph slot 真实尺寸(不强制 cell)
- **GlyphKey** = (font_id, glyph_id, size, weight, italic)
- **shaping**:无.每字符独立查 glyph,kerning 关
- **mono 假设**:每 cell 固定 advance,glyph quad 居中 cell
- **emoji**:走 color atlas,scale 到 cell 大小
- 单纯 mono 终端,够用 —— 但跟 chrome 共用?**Alacritty 没 chrome**(无菜单/无 sidebar/无 tab strip),所以这个问题它不存在

### 2.2 WezTerm

- **rasteriser**:freetype + (mac 可选)CoreText
- **shaping**:HarfBuzz —— 全 Unicode shaping + 复杂脚本 + bidi
- **atlas**:per-glyph slot 真实尺寸 + bearing
- **GlyphKey** = (font_id, glyph_id, size, weight, style, allow_ligatures)
- **proportional UI**:专门走 shaped run path,glyph quad 用 shaping pos
- **mono cell**:CTLine 一次 shape 整行,然后强制每个 glyph 落在 cell 边界(disable kerning + 关 ligature)
- **变体**:支持 variable font 通过 freetype variation axis
- **emoji**:color atlas + ZWJ 序列(HarfBuzz cluster)
- **驱逐**:atlas 满时 LRU
- **结论**:这是 industry 最完整的 implementation,marspot 应该靠拢

### 2.3 Kitty

- **rasteriser**:freetype + CoreText
- **shaping**:HarfBuzz(但是只 enabled for cell-aligned mode)
- **atlas**:per-glyph slot 真实尺寸,但 chrome 仍 mono(命令行参数风)
- **特色**:emoji + 大字符 cell 扩展(`cell_height_metric`)
- 没真正的 proportional UI,所以避开了 chrome 跟 PTY 字体差异的设计问题

### 2.4 iTerm2

- **rasteriser**:CoreText
- **shaping**:CTLine for 复杂脚本,简单 ASCII 跳过
- **atlas**:per-glyph slot
- **chrome**:走 Cocoa 原生(NSTextField / NSMenu),不在 GPU pipeline 上 —— 用 view hierarchy 拼出来,完全跳开 atlas
- **思路**:**chrome 跟 PTY 用完全独立 pipeline**(Cocoa view + GPU PTY)
- marspot 走的是另一条路:chrome 跟 PTY 都过 GPU,统一 atlas —— 但 atlas 必须能容纳 proportional

### 2.5 Skia(Chrome + Flutter 底层)

- **rasteriser**:freetype + Skia 自家管线(支持 hinting / subpixel AA / gamma)
- **shaping**:HarfBuzz
- **atlas**:per-glyph 真实尺寸 + subpixel offset 维度(4 buckets per axis)
- **GlyphKey** = (typeface_id, glyph_id, size, flags, subpixel_offset)
- **驱逐**:LRU
- **正经全功能**:variable fonts / color emoji / svg-in-otf / hinting modes
- 太重 —— marspot 不需要全 Skia,但学它的 cache key 模型

### 2.6 cosmic-text / parley / swash(Rust 生态)

- **shaping**:swash(based on rustybuzz)
- **rasteriser**:swash 自家 + freetype/ab_glyph
- **atlas**:per-glyph + subpixel
- **variable**:OpenType 变体 axis(swash 支持)
- **emoji**:partial(COLR/CPAL 支持,有限 bitmap)
- 跨平台,marspot 未来 Linux/Windows 走这条

### 2.7 CoreText(Apple 原生)

- **shaping**:CTLine —— 收一段 NSAttributedString,出一组 CTRun(每 run 同 font + 同 attribute)
  - 每 CTRun:glyph 数组 + position 数组 + advance 数组
  - 自动处理 kerning / ligature / bidi / 字体回退 / 复杂脚本
- **rasteriser**:CTFontDrawGlyphs / CGContextShowGlyphsAtPositions(我们用的就是这条)
- **emoji**:CTFont.symbolic_traits() 带 kCTFontColorGlyphsTrait,直接走 color path
- **variable**:CTFontCreateWithName + descriptor 设 axis 值
- macOS-only,但 marspot 主要平台 = macOS,先用 CoreText,backend 抽象出口子

### 2.8 共识取舍表

| 维度 | 单 mono 终端(Alacritty / iTerm2) | 全功能 UI 引擎(Skia / Cosmic / WezTerm) | marspot 取哪 |
|---|---|---|---|
| atlas slot | per-glyph 真实尺寸 | per-glyph 真实尺寸 | 真实尺寸 |
| GlyphKey size 维度 | 单一固定 | 多 size 多 weight | **多 size**(chrome 13pt + PTY 12pt + 可能 modal 11pt) |
| GlyphKey subpixel | 无 | 4 或 8 buckets | v1 无,v2 加 4 buckets |
| shaping | 无 / 简单 | HarfBuzz / CTLine | **CoreText CTLine**(macOS-only path) |
| emoji | 独立 color atlas | 独立 color atlas | 已有,保留 |
| variable font | 无 | 通过 axis descriptor | v2 加 |
| LRU | 无 / rebuild | LRU + 容量管理 | v2 LRU |
| chrome 路径 | 独立(Cocoa)| 同 atlas | **同 atlas**(marspot 选择) |

---

## 3. 设计原则

1. **atlas 是 size-agnostic 的资源池** —— 不假定 cell.调用方提供 size,atlas 返真实 bbox.
2. **rasterise 跟 shape 分两层**.rasteriser 接收 `(font, glyph_id, size, subpx_offset)` 出 bitmap.shaping 接收 `(font, text)` 出 `[(glyph_id, pos)]` 序列.
3. **mono cell-aligned 是上层约束,不是 atlas 约束**.PTY 路径在 shaping 跟 quad 提交时强制 cell.
4. **同 atlas 共用,但 mono 跟 ui 用不同 size 的 GlyphKey** —— 这样 PTY 12pt Monaco 跟 chrome 13pt SF Pro 各占各的 cache 条目,互不干扰.
5. **proportional 走 CTLine shape;mono 走 char-by-char**.两种路径都查同一 atlas.
6. **driver 渐进**.每阶段都能 build + ship,逐步替换.

---

## 4. glyph cache 键值 v5

```rust
pub struct GlyphKey {
    pub font_id:        u32,    // FontRegistry idx
    pub glyph:          u16,    // CGGlyph
    pub size_q:         u16,    // 量化字号:round(size_pt × 4)(0.25-pt 精度)
    pub subpx_x:        u8,     // 子像素 x 偏移,0..=N-1(N=4 或 8;v1=1)
    pub flags:          u8,     // bit0 = hint_full;bit1 = subpx_aa;bit2 = small_caps;...
}
```

- `size_q` —— PTY 12pt 跟 chrome 13pt 各占自己的条目.同 glyph 跨 size 共享不可能(rasterise 不同)
- `subpx_x` —— v1 = 1(无 subpixel,等同 0);v2 加到 4 或 8
- `flags` —— rasteriser 模式(hinting / subpixel-AA / synthetic bold).8 bit 足够,扩展性好

```rust
pub struct AtlasEntry {
    pub u0/v0/u1/v1: u16,      // atlas pixel coords
    pub px_w/px_h:    u16,      // 真实 bitmap 尺寸
    pub bearing_x:    i16,      // glyph left side bearing(px,从 origin 到 glyph 左缘)
    pub bearing_y:    i16,      // baseline 到 glyph 顶缘(px,正=向上)
    pub advance_x:    i16,      // 整数 advance(px)— 备用,proportional 实际 advance 走 shape 输出
}
```

**关键**:bearing_x/y 让上层正确放 glyph(放在 baseline 相对位置).现有 AtlasEntry 没有,因为 SlotMetrics 帮上层算好了 cell-aligned 位置 —— v5 把决定权交给上层(shaping 或 mono cell 强制).

---

## 5. atlas 分配 v5

- 保留 shelf packer(简单 + 已有)
- shelves 按 bucket-rounded 高度分组(eg 8/16/24/32 px 桶)—— 高度近的 glyph 聚集
- 加 LRU 链表:每个 AtlasEntry 维护 `last_used: u64`(frame_id)
- 满时:**驱逐最久未用的整个 shelf**,腾位置(per-entry 驱逐成本高,shelf 粒度足够)
- 不再无差别 rebuild(0.6.x 现状)—— 那 invalidates 整批 cache 一帧 stutter

接口:

```rust
impl GlyphAtlas {
    pub fn get_or_rasterize(
        &mut self,
        key: GlyphKey,
        rasterise: impl FnOnce() -> Option<RasterOutput>,  // 闭包 — 仅 miss 时调
        frame: u64,
    ) -> Option<AtlasEntry>;
}

pub struct RasterOutput {
    pub bytes: Vec<u8>,
    pub px_w/px_h: u16,
    pub bearing_x/y: i16,
}
```

- 闭包形式:cache hit 路径零 alloc 零 syscall
- frame 是当前帧序号,用来更新 LRU 时戳

---

## 6. raster 流水

```rust
fn rasterise_glyph(
    font: &CTFont,
    glyph: CGGlyph,
    size_px: f32,
    subpx_x: f32,
    flags: u8,
) -> Option<RasterOutput> {
    let bbox = font.get_bounding_rects_for_glyphs(...);
    if bbox.size <= 0 { return None; }

    // 添 1px padding 避免 linear sampling 渗边
    let px_w = (bbox.size.width).ceil() as u32 + 2;
    let px_h = (bbox.size.height).ceil() as u32 + 2;

    let mut bytes = vec![0u8; (px_w * px_h) as usize];
    let ctx = CGBitmapContext::new(...);
    ctx.set_should_antialias(true);
    ctx.set_should_smooth_fonts(flags & FLAG_SMOOTH != 0);
    ctx.set_should_subpixel_position_fonts(false);  // 我们自己 quantize subpx
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    ctx.set_gray_fill_color(1.0, 1.0);

    // draw glyph at (bearing_offset + subpx, baseline_in_bitmap)
    let dx = -bbox.origin.x + (subpx_x as f64) + 1.0;          // 含 1px pad
    let dy = px_h as f64 - (bbox.origin.y + bbox.size.height + 1.0);  // y-up
    font.draw_glyphs(&[glyph], &[CGPoint { x: dx, y: dy }], &ctx);

    Some(RasterOutput {
        bytes,
        px_w: px_w as u16,
        px_h: px_h as u16,
        bearing_x: (bbox.origin.x as i16) - 1,
        bearing_y: ((bbox.origin.y + bbox.size.height) as i16) + 1,
    })
}
```

跟现状对比:
- 不接收 `SlotMetrics` —— bbox 决定 bitmap 尺寸
- 不强制 cell_w × cell_h 大小
- 不强制 baseline 行
- 输出 bearing_x/y 让上层正确放置

---

## 7. shaping 流水(UI 路径)

```rust
pub struct ShapedRun {
    pub font_id:  u32,
    pub glyph_id: u16,
    pub x_advance: f32,    // shape 给的 proportional advance(浮点)
    pub x_offset:  f32,    // shape 局部 dx(kerning / glyph cluster fix-up)
    pub y_offset:  f32,
}

pub fn shape_line_via_ctline(
    text: &str,
    style: &TextStyle,
) -> Vec<ShapedRun> {
    // 用 CoreText 全自动:
    let attr_str = NSAttributedString::new(text, attrs_with_font_color(style));
    let line = CTLine::create_with_attributed_string(attr_str);
    let mut out: Vec<ShapedRun> = Vec::new();
    for run in line.glyph_runs() {
        let font = run.font();
        let font_id = font_registry.intern(font);
        let glyphs = run.glyphs();
        let positions = run.positions();
        let advances = run.advances();
        for i in 0..glyphs.len() {
            out.push(ShapedRun {
                font_id, glyph_id: glyphs[i],
                x_advance: advances[i].width as f32,
                x_offset: 0.0, y_offset: 0.0,  // CTRun position 已含偏移
            });
        }
    }
    out
}
```

**关键**:CTLine 帮我们做:
- 字体回退(CJK / emoji 自动落到 fallback)
- kerning / ligature(SF Pro 的 connecting `fi` 自动连)
- bidi(LTR + RTL 混排)
- 复杂脚本

我们要做的:每个 run 找到对应的 FontRegistry idx + 查 atlas + 提交 GlyphInstance.

---

## 8. mono 终端 grid 路径(向后兼容)

PTY 必须保持 cell-aligned —— 否则相邻行 baseline 漂,会被觉察为"晕".v5 路径:

```rust
// PTY 一行一行 paint(已是这条 path)
for cell in row {
    let ch = cell.ch;
    let n_cells = char_width(ch);  // 1 or 2
    let (font_id, glyph) = font_cache.resolve_char(ch, bold, italic);
    let entry = atlas.get_or_rasterize(
        GlyphKey { font_id, glyph, size_q: PTY_SIZE_Q, subpx_x: 0, flags: 0 },
        || rasterise_glyph(font, glyph, PTY_SIZE_PX, 0.0, 0),
        frame_id,
    )?;

    // ★ 强制 cell-aligned:
    // - quad 宽 = cell_w × n_cells(忽略 entry.px_w)
    // - quad 高 = cell_h
    // - glyph 在 quad 内的位置 = ((cell_w × n_cells - entry.px_w) / 2,
    //                              ascent - entry.bearing_y)
    //   也就是水平居中 + 竖直对齐 baseline
    let quad_x = cell_x + (cell_w * n_cells as f32 - entry.px_w as f32) * 0.5;
    let quad_y = baseline_y - entry.bearing_y as f32;

    glyphs.push(GlyphInstance {
        origin: [quad_x.round(), quad_y.round()],   // 整数,baseline 对齐
        size:   [entry.px_w as f32, entry.px_h as f32],  // 真实 bbox
        uv: [entry.u0/v0/u1/v1],
        color,
    });
}
```

- atlas slot 真实 bbox(共用 chrome 那条 cache 条目)
- 提交时强制 cell-grid 位置
- baseline_y 计算保留整数:`row_top + ascent_q`

这样:
- mono 视觉:**跟现状一致**(glyph 居中 cell,baseline 对齐)
- 但 atlas 不再浪费整个 cell 空间(narrow glyph 实际尺寸入 atlas)
- 共享 atlas 跟 chrome,size_q 不同所以无碰撞

---

## 9. subpixel 位置(可选)

proportional 字体 sub-pixel 位置不对齐时,字符间距看上去抖.正常解:

- 量化 glyph x 位置到 `subpx_x ∈ {0, 1/N, 2/N, ..., (N-1)/N}` 桶(N=4 或 8)
- atlas 为每 subpx_x 缓存一份 raster
- raster 时:draw glyph at `bitmap_x + subpx_x`
- 提交 quad 时:`origin.x = floor(true_x) + subpx_x * 0`(取整,glyph 内部已偏移)

代价:
- N=4:atlas 用量 ×4
- N=8:×8
- v1 默认 N=1(无 subpixel),旧行为
- v2 升 N=4,跟 chrome SF Pro 配套

---

## 10. color emoji

现有 color_atlas(BGRA8Unorm)已对.v5 只改:
- color path 也走 per-glyph 真实尺寸(不再 force cell)
- emoji bitmap 通常 100×100 左右,跟 cell(8×16 px 12pt scale)差大 —— 现状是 scale fit;v5 改为 atlas 装真尺寸,提交 quad 时 mono PTY 强制 cell 大小(scale at GPU),chrome 直接用真尺寸
- 通过 GlyphKey.flags 区分 color/mono atlas dispatch(目前看 `is_color_font(idx)` 走两条 atlas;留这条逻辑)

---

## 11. variable font

CoreText 支持 axis variation:

```rust
let descriptor = CTFontDescriptor::create_with_axes(&[
    (kCTFontWeightTrait, weight_value),   // 100..1000
    (kCTFontOpticalSize, optical_size),   // 跟 size_pt 联动
]);
let font = CTFont::with_descriptor(descriptor, size_pt);
```

每个不同 (weight, size, ...) 组合 intern 成新 FontRegistry idx.GlyphKey.font_id 区分.atlas 自动隔离.

- SF Pro 的 weight 100/400/600/700 各自 intern → 4 个 font_id
- 用户切 weight → 用对应 font_id 查 atlas,cold miss 走 raster

---

## 12. OpenType 特性

CTLine shape 默认 ON:
- kerning(`kern`)
- ligature(`liga`)
- contextual alt(`calt`)

通过 NSAttributedString attribute 关:
- terminal 路径:`NSLigatureAttributeName = 0` 关 ligature(每 cell 独立)
- mono 路径:不走 CTLine,无 kerning(每 cell 等宽推进)
- chrome 路径:全 ON

---

## 13. atlas 驱逐

现状:满时 rebuild 全部.cache clear → 下一帧整批 raster → 一帧大 stutter.

v5:
- 每 AtlasEntry 加 `last_used: u64`(frame_id)
- get_or_rasterize 命中时更新 last_used
- 满时:挑 last_used 最小的整个 shelf 驱逐
- shelf 粒度:简单 + 缓存有效率高(相同高度 glyph 同一 shelf,driven 工作集进退一致)

进一步:
- frame_id 每帧递增(u64 不会 wrap 实际)
- 每帧开始时通知 atlas `begin_frame(frame_id)`(为驱逐统计)

---

## 14. 跨平台预备

`rasteriser` 跟 `shaper` 抽象 trait:

```rust
pub trait Rasteriser {
    fn rasterise(&self, key: GlyphKey) -> Option<RasterOutput>;
    fn intrinsic_advance(&self, font_id: u32, glyph: u16, size_px: f32) -> f32;
}

pub trait Shaper {
    fn shape_line(&mut self, text: &str, style: &TextStyle) -> Vec<ShapedRun>;
}
```

macOS impl:CoreText backend(本文 §6/§7).Linux backend = rustybuzz(shape)+ ab_glyph 或 fontdue(raster).Windows = DirectWrite.

trait 是 v3+ 的事 —— marspot v1 macOS only,直接调 CoreText API,trait 留为代码结构线.

---

## 15. 4 阶段 migration plan

### Phase 1 — atlas 改 per-glyph 真实 bbox(2-3 commits)

- AtlasEntry 加 `bearing_x/y`
- get_or_rasterize 改:不接 SlotMetrics;raster 输出真实 bbox
- 不变更 PTY paint —— 上层 PTY 路径强制居中 cell + 用 entry.bearing_y 算 baseline
- chrome 路径仍走 mono(目前的 0.6.29 状态),但 atlas 已 per-glyph
- 验证:PTY 视觉无变化;atlas 利用率提高(窄字符不再浪费 cell 宽)

### Phase 2 — chrome 走 CTLine shape(proportional 真接通)

- shape_line_via_ctline 实现
- push_text_run_kind(Ui) 改:shape 整段 text → 提交 glyph 序列,position 用 shaper 给的 advance(浮点)
- mono PTY 路径不变(仍 char-by-char + cell 强制)
- GlyphKey 加 `size_q`(PTY 跟 chrome size 区分)
- 验证:dev panel SF Pro 正确 proportional 渲染,字符不变形,kerning 自动(eg `Tay` 中 'T' 和 'a' 紧贴)

### Phase 3 — subpixel positioning(可选,视觉精细化)

- GlyphKey.subpx_x 启用(N=4)
- raster 加 subpx_x 偏移
- shape 输出量化 quantize 到 1/4 px
- atlas 4× 用量(注意监控驱逐率)
- 验证:proportional text 在小字号(11-13pt)下文字粘连感消失

### Phase 4 — LRU 驱逐 + 多 size + variable font

- atlas LRU shelf 驱逐
- FontRegistry 加 size 维度:`intern_at_size(font, size_pt) -> font_id`
- variable font:axis descriptor → 多 font_id
- 验证:atlas 驱逐统计稳定,无 stutter;chrome+PTY+modal 多 size 共存

---

## 16. 待解问题

1. **subpixel AA**(LCD)在 Apple Silicon 上 macOS 10.14+ 已 deprecated —— iTerm2/Terminal.app 都走 grayscale.我们应该跟着走 grayscale.确认.

2. **font smoothing** 现在 ON(CoreText stroke widening).换 atlas 后是否仍需要,需 visual 验证.

3. **emoji ZWJ 序列**(👨‍👩‍👧‍👦)是 CTLine 输出单 cluster 单 CTRun.atlas 需把整 cluster 作一个"big glyph"还是分子 glyph?CoreText 给的是 base glyph + 多 attachment.建议:第一个 base glyph 是结果 ZWJ glyph,CTLine 已合并.我们查 atlas 时用这个合成 glyph_id.

4. **CTLine 每帧 shape 全部 chrome text** —— 性能?预估 chrome 共 ~1000 字符,每帧 CTLine 创建 + iterate = ?ms 估算.若超 1ms 加 string-level cache(同 text → 缓存的 ShapedRun 列表).key = (text, size_q, font_id, weight) hash.

5. **atlas 维度多大够**?现在 1024×1024 ≈ 1MB R8.加 chrome 字号 + subpx → 估算最大工作集 4-8 MB.单 atlas 4096×4096(16 MB)起步.

6. **glyph cluster width 决定权**:现状 char_width(ch) 决定 PTY n_cells.但 CoreText 给的 advance 可能不一致(CJK 字 advance = 2× cell_w,但 emoji 经常是不规则).PTY 路径继续 trust char_width;chrome 路径 trust shape advance.两条不冲突.

7. **fonts re-intern 何时清**?当用户切换 light/dark theme 或换 font 配置,旧 font_id 的 atlas 条目需要驱逐.LRU 自然处理(不再用 = 最先驱逐).

---

## 17. 性能红线

继承 marspot 现有约束:

- **idle CPU 0%** —— atlas 驱逐 / shape 仅在 redraw 时跑,不开后台 worker
- **cold raster < 500us / glyph** —— 跟现状一致(CoreText raster ~200us avg)
- **atlas 查询 O(1) HashMap**(FxHash,跟现状一致)
- **per-frame shape(chrome)< 1ms** —— ~1000 字符,CTLine 单次 shape 估 < 200us;加 cache 可降到 < 50us
- **per-frame shape(PTY)= 0**,仍 char-by-char
- **GPU 提交时间不变** —— atlas slot 改成真实 bbox 之后,quad 数量可能略多但单 quad 更小,差几乎为零

---

## 18. 一句话

**marspot font v5 = CoreText shaping(chrome 走 CTLine,PTY 走 char-by-char)+ atlas per-glyph 真实 bbox + GlyphKey(font, glyph, size_q, subpx)+ LRU 驱逐 + (v3+)变体跨平台抽象.同 atlas 服务 mono 跟 proportional 不打架.PTY 视觉 0 漂移,chrome 真 proportional 不变形.**
