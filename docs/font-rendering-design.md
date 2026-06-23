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
- `subpx_x` —— ship-time **强制 4 buckets**(0.25 px 精度;PTY 路径恒传 0,chrome shape 输出后会带 0..3 任一桶)
- `flags` —— rasteriser 模式.bit0=smooth(stroke widening),bit1=subpx_aa(关,Apple Silicon grayscale only),bit2=synthetic_bold(fallback 字体无 bold variant 时合成),bit3=synthetic_italic.8 bit 足够

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

## 9. subpixel 位置(强制 4 buckets)

proportional 字体 sub-pixel 位置不对齐时,字符间距看上去抖.正常解:

- 量化 glyph x 位置到 `subpx_x ∈ {0, 1/N, 2/N, ..., (N-1)/N}` 桶(N=4 或 8)
- atlas 为每 subpx_x 缓存一份 raster
- raster 时:draw glyph at `bitmap_x + subpx_x`
- 提交 quad 时:`origin.x = floor(true_x) + subpx_x * 0`(取整,glyph 内部已偏移)

决定:
- **N = 4**(0.25 px 精度),v5 ship-time 强制开
- atlas 用量 ×4 —— 在 §16 Phase 2 给 atlas 提升到 4096² 时一并预留
- shape 输出位置量化:`shape_x_quantized = floor(shape_x) + bucket / 4`,bucket = `round((shape_x - floor(shape_x)) × 4) % 4`
- raster 时偏移 bitmap 对应 sub-pixel:`draw_glyph_at(x = pad + bucket × 0.25, y = baseline_in_bitmap)`
- 实测 11-13pt SF Pro chrome 文字密集排版 —— subpx 关 vs 开有可观察的字符黏连差异;关 = ship 不能接受

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

## 13. atlas 驱逐(LRU shelf-粒度)

现状:满时 `rebuild` 整个 cache 清 + 下一帧整批 raster → 一帧大 stutter.

v5(Phase 6,ship-time 强制):
- 每 `AtlasEntry` 加 `last_used: u64`(frame_id)
- `Shelf` 也维护 `last_used = max(entries.last_used)`
- `get_or_rasterize` 命中时更新 entry + shelf 的 last_used
- 满时:挑 last_used 最小的整个 shelf 驱逐,移除 shelf 跟其 entries
- shelf 粒度:per-entry 太碎(碎片化 + 内存洞洞),shelf 粒度好(相同高度 glyph 工作集进退一致)
- `begin_frame(frame_id)` —— 每帧开始通知 atlas;driver 用 u64 frame counter(u64 不会 wrap,大概 5000 万年)
- `rebuild` 方法删除 —— v5 不允许"清光"路径,任何驱逐必走 LRU shelf
- `rebuild_count` 字段保留 + 永久应 = 0(出现非零 = 红线被破)

```rust
impl GlyphAtlas {
    pub fn begin_frame(&mut self, frame_id: u64) { self.current_frame = frame_id; }
    fn evict_oldest_shelf(&mut self) -> bool {
        let oldest = self.shelves.iter()
            .min_by_key(|s| s.last_used)?;
        let oldest_idx = ...; // index of oldest
        // remove shelf + invalidate cache entries pointing into its y range
        self.cache.retain(|_, e| !shelf_contains(oldest, e));
        self.shelves.swap_remove(oldest_idx);
        true
    }
}
```

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

## 15. 完整支持矩阵(font v5 ship 后必须全绿)

每行 = 一类字体 / 一类特性,每列 = 渲染场景.✓ = ship-time 必须工作.

|  | PTY 终端 | chrome / dev panel | modal / popover | 状态栏 |
|---|---|---|---|---|
| **Mono 编程字体**(Monaco / SF Mono / Menlo / Hack / JetBrains Mono / Fira Code / Cascadia Code) | ✓ | ✓ | ✓ | ✓ |
| **Proportional UI**(SF Pro / Helvetica / Arial / 系统字体) | n/a | ✓ kerning + ligature | ✓ | ✓ |
| **Variable font**(SF Pro / Inter Variable;weight 100-900 + optical_size 轴) | n/a | ✓ 任意 weight | ✓ | ✓ |
| **CJK 中日韩**(PingFang / Hiragino / AppleSDGothicNeo / Noto CJK) | ✓(2 cells) | ✓(shape 给真 advance) | ✓ | ✓ |
| **Italic / Bold 变体** | ✓ | ✓ | ✓ | ✓ |
| **Color emoji**(Apple Color Emoji) | ✓(2 cells scale fit) | ✓(真尺寸) | ✓ | ✓ |
| **Emoji ZWJ 序列**(👨‍👩‍👧‍👦) | ✓(单 cluster 2 cells) | ✓(单 atlas entry) | ✓ | ✓ |
| **Programming ligature**(`==>`、`!=` 连字) | ✗(每 cell 独立,故意关) | ✓(chrome 显示 / docs / hints) | ✓ | ✓ |
| **Symbol 字体**(SF Symbols / 自定义 ttf) | ✓(per char) | ✓ | ✓ | ✓ |
| **Bidi**(LTR + RTL 混排,Arabic / Hebrew) | n/a(grid 永远 LTR) | ✓(CTLine 自动处理) | ✓ | ✓ |
| **Combining marks**(é = e + ◌́;CJK 注音符号) | ✓(单 cluster) | ✓(单 cluster) | ✓ | ✓ |
| **小字号**(8-11pt) + **大字号**(16-32pt) | n/a(单一 PTY size) | ✓(任意 size_q) | ✓ | ✓ |
| **Theme / Font 运行时切换**(用户改字体 → 实时换) | ✓(LRU 自然驱逐旧) | ✓ | ✓ | ✓ |

**任何一格 ✗ 都不算 v5 完成**.这是 "完整字体支持" 的硬定义.

---

## 16. 10 阶段 mandatory migration plan(无 defer,无 optional)

每阶段一个 ship-able commit / 一组 commits,独立可回滚.全 10 阶段完成 = font v5 ship.预估总工时 3-4 周(per-phase 1-3 工作日).

### Phase 1 — atlas per-glyph 真实 bbox + bearing(基石)

**改动**:`src/glyph_atlas.rs` + `src/font_cache.rs`
- `AtlasEntry` 加 `bearing_x: i16, bearing_y: i16`
- `RasterOutput` 引入(替代部分 SlotMetrics 职责)
- `rasterise_glyph` 输出真实 bbox + bearing,不再 force cell_w × cell_h
- `get_or_rasterize` 闭包形态:`FnOnce() -> Option<RasterOutput>`,cache hit 零开销
- 兼容:`push_text_run`(PTY 路径)在 paint 时算 `quad_x = cell_x + (cell_w × n - bbox_w) / 2`,`quad_y = baseline - bearing_y`;视觉跟现状一致

**Acceptance**:
- PTY 12pt Monaco bench 渲染像素 diff = 0(对照现状基线)
- atlas 利用率从现 ~60% 提升到 ~85%(窄字符不再撑满)
- 9 session 终端 grid bench p99 不退化(< 1300µs 维持)

**Risk + rollback**:atlas 数据结构破坏性变更 — 旧 cache 序列化数据不兼容;atlas 是运行时构造,无持久化,无 rollback 问题

---

### Phase 2 — GlyphKey 加 size_q + flags(多 size 解锁)

**改动**:`src/glyph_atlas.rs`
- `GlyphKey = (font_id, glyph, size_q: u16, subpx_x: u8, flags: u8)`
- size_q = `round(size_pt × 4)`(0.25-pt 精度)
- subpx_x 暂留位(默认 0),Phase 4 启用
- flags:bit0=smooth, bit1=subpx_aa(off on Apple Silicon)
- atlas 容量 1024² → 4096²(16 MB R8 + 4 MB BGRA8 emoji)

**Acceptance**:
- PTY 12pt + chrome 13pt 共存,各自独立 cache,不互踩
- atlas dim bump 完后 bench 一致 / 无回退

---

### Phase 3 — CTLine shape(chrome 真 proportional 接通)

**改动**:`src/render_metal.rs` + 新 `src/font_shape.rs`
- 新 module `font_shape`:`shape_line_via_ctline(text, style) -> Vec<ShapedRun>`
  - 用 `CTFramesetterCreateWithAttributedString` 或更直接的 `CTLineCreateWithAttributedString`
  - 遍历 CTRun → glyph + position + advance + font + flags
- `push_text_run_kind(Ui)` 整段 shape 一次,提交 glyph 序列
- mono PTY 路径不动
- string-level shape cache(`(text_hash, size_q, font_idx, weight)` → `Vec<ShapedRun>`,LRU 1024 条)
  - **必须**,不是 optional —— 没 cache 每帧 chrome shape 1000 字符成本高

**Acceptance**:
- dev panel SF Pro 字符不变形,字距自然(kerning 自动:'Ta' 紧贴)
- `Tax`/`fi` ligature ON,visible diff vs Phase 2(只字符外形,无 kerning)
- chrome shape per frame < 1ms(cache hit < 50µs)
- CJK 中文混排 chrome ↔ fallback 正确(SF Pro 无中文 → PingFang)

---

### Phase 4 — subpixel positioning(强制开,4 buckets)

**改动**:`src/glyph_atlas.rs` + `src/font_shape.rs`
- `subpx_x: u8 ∈ 0..4`,key 自动包含
- raster 时 `dx_in_bitmap += subpx_x × 0.25`
- shape 输出位置量化:`quantized_x = floor(x) + subpx_bucket / 4`
- atlas 用量 ×4(已在 Phase 2 4096² 预留)

**Acceptance**:
- proportional text 在小字号(11-13pt)字符**不黏连**:连续 'i'/'l' 不撞
- subpx 关 vs 开做 visual diff,差异主要在小号 chrome 字
- atlas 驱逐率监控:仍 < 1 evict / sec idle

---

### Phase 5 — variable font 支持

**改动**:`src/font_cache.rs`
- `FontRegistry::intern_with_axes(font_name, size, axes)` —— 接 `(kCTFontWeightTrait, 0.0..1.0)` 等
- 标准 weight 档(100/200/.../900)预 intern + cache
- `resolve_char_styled(ch, weight, italic, condensed)` 返带 axis 的 font_id

**Acceptance**:
- SF Pro 任意 weight(100 light → 900 black)可渲染
- Inter Variable 安装时同样工作
- chrome dev panel `h1` 用 weight=600,`body` 用 400 —— visible diff

---

### Phase 6 — LRU 驱逐(替代 rebuild_all)

**改动**:`src/glyph_atlas.rs`
- `AtlasEntry.last_used: u64`
- atlas 接 `begin_frame(frame_id)` —— 每帧开始通知
- `get_or_rasterize` 命中更新 last_used
- full 时:选 last_used 最小的 shelf 整 shelf 驱逐(per-entry 太碎,shelf 粒度好)
- `rebuild_count` 仍记录(应趋零)

**Acceptance**:
- 60s 终端高强度输入 + 切 panel 跑下来,`rebuild_count` 增量 = 0
- 旧 rebuild_all path 删除
- atlas 满时驱逐 stutter < 5ms(单 shelf eviction)

---

### Phase 7 — color emoji 完整(per-glyph bbox + ZWJ)

**改动**:`src/glyph_atlas.rs` color path
- `rasterise_glyph_color` 输出真实 bbox(类似 mono path)
- ZWJ cluster:CTLine 已合并到单 glyph_id,atlas 单 entry 缓存
- PTY 路径:single emoji glyph_id rendered into 2 cells,quad 强制 cell 大小 + scale fit
- chrome 路径:emoji 直接真实 bbox 渲染(可能 16×16 px 真大小)

**Acceptance**:
- 终端 `👨‍👩‍👧‍👦` 单 grapheme(2 cells)
- chrome `👍🏼` 显示完整 5-glyph ZWJ 序列,无 .notdef
- 性别 / 肤色 modifier 处理正确

---

### Phase 8 — OpenType feature 控制(per-context 开关)

**改动**:`src/font_shape.rs`
- `ShapeOptions { kerning: bool, liga: bool, calt: bool, contextual: bool }`
- PTY: `ShapeOptions::all_off()` — 每 cell 独立,不连
- chrome body: `ShapeOptions::default()` — 全开
- code block: `ShapeOptions::code()` — kerning 关 但 liga 开(`==>` 显示连字)
- 通过 `NSAttributedString` attributes 设置(`kCTLigatureAttributeName` 等)

**Acceptance**:
- 终端 Fira Code 输入 `==>` 仍三字符独立显示
- chrome dev panel 中显示同 `==>` 显示连字
- chrome `Tax` 字距紧凑(kerning ON)
- PTY `Tax` 三字独立等宽

---

### Phase 9 — visual regression test matrix + bench gate

**改动**:`bench/font-rendering/` + `bin/font-bench.sh`
- 12 个标杆 string(各 font 类 × 各 场景):
  - "The quick brown fox jumps over the lazy dog 0123456789"
  - "你好世界 こんにちは 안녕" CJK 混排
  - "👨‍👩‍👧‍👦🍕🇯🇵" emoji + ZWJ + flag
  - "Tax fi fl ff ==> != !==" kerning / ligature
  - "Bidi: Hello مرحبا עברית" RTL 混排
- 渲染到 PNG(headless)
- 对比基线 PNG(SSIM > 0.98)
- bench:每场景 cold-raster + warm-cache 时延

**Acceptance**:
- 12/12 visual diff < 2%(允许 AA 噪音)
- cold raster < 500µs / glyph
- warm cache lookup < 100ns / glyph
- shape per frame < 1ms 

---

### Phase 10 — 跨平台抽象 trait 提取

**改动**:`src/render/font/` 新 sub-module
- `pub trait Rasteriser { fn rasterise(&self, key: GlyphKey) -> Option<RasterOutput>; }`
- `pub trait Shaper { fn shape(&mut self, text: &str, style: &TextStyle) -> Vec<ShapedRun>; }`
- macOS impl:`CoreTextRasteriser` + `CoreTextShaper`(从现有 path 抽出)
- Linux/Windows impl 留接口,实现时再补(rustybuzz + ab_glyph / DirectWrite)

**Acceptance**:
- 编译 trait + impl,所有调用经 trait
- 单元测试:headless `MockRasteriser` 可 plug 进 atlas 跑(不实际 raster)
- 文档:`docs/font-rendering-design.md` §14 跨平台具体化为 trait 签名

---

## 17. 已决定的 trade-off(无遗留问题)

旧版 §16 列了 7 个"待解问题",这里逐一拍板:

| # | 问题 | 决定 |
|---|---|---|
| 1 | LCD subpixel AA? | **关**.Apple Silicon macOS 10.14+ 已停 LCD subpx,grayscale AA only. |
| 2 | font_smoothing(CT stroke widening)? | **保持 ON**.Phase 1 后 visual diff 验证未退化即可. |
| 3 | emoji ZWJ atlas storage? | **单 cluster 单 atlas entry**.CTLine 合并,glyph_id 是合成 id. |
| 4 | CTLine 每帧 shape 性能? | **string-level shape cache**(LRU 1024 条).Phase 3 mandatory. |
| 5 | atlas 维度? | **4096² R8**(16 MB)+ **4096² BGRA8**(64 MB)emoji.Phase 2 一次到位. |
| 6 | PTY n_cells vs shape advance 冲突? | **PTY 信 `char_width()`,chrome 信 shape advance**.两条不共享决定权. |
| 7 | font re-intern 清理? | **LRU 自然驱逐**(Phase 6).不需额外 GC. |
| 8 | subpixel buckets(2/4/8)? | **4 buckets**(0.25 px 精度).平衡 atlas 用量 vs 视觉. |
| 9 | shape cache key 包含 color/alpha? | **不包含**.color 是 GlyphInstance 属性,跟 glyph 形状无关. |
| 10 | text cache 命中失效何时清? | **theme version() 变 / set_font() / chrome 字体改时全清**.集成 [A1] theme hook. |

---

## 18. Acceptance 红线(全部满足 = v5 ship)

PTY 视觉:
- ✓ Monaco 12pt mono 渲染像素 diff = 0(vs 现状)
- ✓ CJK 中文混排无 baseline 漂移
- ✓ emoji 在 2 cells 正确缩放
- ✓ box drawing `┌─┐│└─┘` 接缝无 hairline gap

chrome 视觉:
- ✓ SF Pro 字符不变形,真 proportional
- ✓ kerning + ligature 自动(`Tax` / `fi` / `==>`)
- ✓ subpixel 小字号(11-13pt)无字符黏连
- ✓ variable font weight 100→900 渐变
- ✓ CJK chrome 文本 fallback 正确(SF Pro → PingFang),baseline 对齐

跨字体兼容(§15 矩阵):
- ✓ 12/12 visual regression PNG SSIM > 0.98
- ✓ 13 类字体特性全部支持

性能:
- ✓ idle CPU = 0(无 anim 时)
- ✓ PTY p99 render < 1.3ms(现状基线 + Phase 1 不退化)
- ✓ chrome shape per frame < 1ms(cache hit < 50µs)
- ✓ cold raster < 500µs / glyph
- ✓ warm cache lookup < 100ns / glyph

资源:
- ✓ 9 session steady-state atlas evict < 1 / 秒
- ✓ rebuild_count = 0(Phase 6 后)
- ✓ atlas 总占用 < 32 MB(R8 + BGRA8)

---

## 19. 风险 + rollback

每 Phase ship-able commit,可独立 revert.最大风险节点:

1. **Phase 1**(atlas 数据结构破坏)— 影响所有 raster path.
   - 风险:PTY 视觉 diff
   - 防控:visual regression test matrix Phase 9 提前 cherry-pick 出基线 PNG;Phase 1 立刻验证
   - rollback:`git revert` 一个 commit

2. **Phase 3**(CTLine shape)— chrome 整个渲染路径改写.
   - 风险:CTLine 用法错 / 性能不达标
   - 防控:string-level cache 提前实现;cache miss 时 measure
   - rollback:`encode_canvas_into` ui_font 参数回 false(已是 0.6.29 状态)

3. **Phase 5**(variable font)— 接 CTFontDescriptor axis,旧 macOS 兼容
   - 风险:macOS 12+ only;旧系统 fallback
   - 防控:axis 缺失时返默认 weight font,不 panic

4. **Phase 7**(emoji ZWJ)— Unicode 复杂,bug 多
   - 风险:某些 ZWJ 序列渲染 broken
   - 防控:测试矩阵覆盖 10 个流行 emoji 组合

---

## 20. 性能红线(无降级保证)

继承 marspot 现有约束:

- **idle CPU 0%** —— atlas 驱逐 / shape 仅在 redraw 时跑,不开后台 worker
- **cold raster < 500us / glyph** —— 跟现状一致(CoreText raster ~200us avg)
- **atlas 查询 O(1) HashMap**(FxHash,跟现状一致)
- **per-frame shape(chrome)< 1ms** —— ~1000 字符,CTLine 单次 shape 估 < 200us;加 cache 可降到 < 50us
- **per-frame shape(PTY)= 0**,仍 char-by-char
- **GPU 提交时间不变** —— atlas slot 改成真实 bbox 之后,quad 数量可能略多但单 quad 更小,差几乎为零

---

## 21. 一句话

**marspot font v5 = CoreText shape(chrome CTLine + PTY char-by-char)+ atlas per-glyph 真实 bbox + bearing + GlyphKey(font, glyph, size_q, subpx, flags)+ subpixel 4 buckets + variable font(weight 100-900 + 任意 axis)+ string-level shape cache + LRU shelf 驱逐 + Color emoji ZWJ cluster + OpenType 特性 per-context 开关 + visual regression SSIM gate + Rasteriser/Shaper trait 跨平台抽象.13 类字体特性 × 4 渲染场景全绿,PTY 0 漂移,chrome 真 proportional 不变形,无 defer 无 optional.**
