// Cell-grid shaders for the Metal renderer (`render_metal.rs`).
//
// Two passes are defined here so they share one MTLLibrary:
//   * BG pass — instanced coloured quads, one per cell.  Phase 3.
//   * FG pass — placeholder for the textured glyph quads.  Phase 4.
//
// Coordinate convention used by both:
//   `origin`/`size` are in physical pixels with the **top-left** as
//   origin and +y pointing **down** — i.e. the same convention the
//   AppKit renderer uses.  The vertex shader does the y-flip into
//   Metal NDC (which is y-up).
//
// Buffer layout for both passes (per-instance):
//   buffer(0) = `device const Cell* cells`
//   buffer(1) = `constant float2& viewport_px`  (set via setVertexBytes)

#include <metal_stdlib>
using namespace metal;

// One cell's drawing data, packed to 32 bytes.  Must match
// `CellInstance` in render_metal.rs exactly.
struct Cell {
    float2 origin;
    float2 size;
    float4 color;
};

struct VOut {
    float4 position [[position]];
    float4 color;
};

// Shared corner offsets — two triangles, CCW winding from Metal's
// view (after the y-flip below).  Marked `constant` so MSL keeps it
// in fast-access constant memory.
constant float2 corners[6] = {
    float2(0.0, 0.0),
    float2(1.0, 0.0),
    float2(0.0, 1.0),
    float2(1.0, 0.0),
    float2(1.0, 1.0),
    float2(0.0, 1.0),
};

vertex VOut bg_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    device const Cell* cells [[buffer(0)]],
    constant float2& viewport_px [[buffer(1)]]
) {
    Cell c = cells[iid];
    float2 px = c.origin + c.size * corners[vid];

    // px → NDC.  px is top-left/y-down; NDC is centre/y-up.
    float2 ndc = (px / viewport_px) * 2.0 - 1.0;
    ndc.y = -ndc.y;

    VOut o;
    o.position = float4(ndc, 0.0, 1.0);
    o.color = c.color;
    return o;
}

fragment float4 bg_fragment(VOut in [[stage_in]]) {
    return in.color;
}

// ----------------------------------------------------------------------
// FG pass — textured glyph quads sampling the alpha-only atlas.
//
// One instance per visible glyph.  `uv0`/`uv1` are normalised
// (0..1) atlas coords; `color` is the foreground tint.  The
// fragment shader multiplies the atlas's alpha into `color.a`
// and lets the pipeline's alpha-blend equation composite onto
// the BG underneath.
// ----------------------------------------------------------------------

struct Glyph {
    float2 origin;
    float2 size;
    float2 uv0;
    float2 uv1;
    float4 color;
};

struct GVOut {
    float4 position [[position]];
    float2 uv;
    float4 color;
};

vertex GVOut fg_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    device const Glyph* glyphs [[buffer(0)]],
    constant float2& viewport_px [[buffer(1)]]
) {
    Glyph g = glyphs[iid];
    float2 px = g.origin + g.size * corners[vid];
    float2 ndc = (px / viewport_px) * 2.0 - 1.0;
    ndc.y = -ndc.y;

    GVOut o;
    o.position = float4(ndc, 0.0, 1.0);
    o.uv = mix(g.uv0, g.uv1, corners[vid]);
    o.color = g.color;
    return o;
}

fragment float4 fg_fragment(
    GVOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]],
    sampler atlas_sampler [[sampler(0)]]
) {
    float coverage = atlas.sample(atlas_sampler, in.uv).r;
    return float4(in.color.rgb, in.color.a * coverage);
}

// Colour-glyph FG fragment — samples the BGRA colour atlas (full-colour
// emoji) and outputs the texel directly.  The atlas stores PREMULTIPLIED
// alpha (the rasteriser drew into a premultiplied context), so the colour
// pipeline blends with source factor One; we scale by in.color.a so pane
// dimming (the only thing the cell colour carries here) still applies —
// scaling a premultiplied colour by a scalar keeps it premultiplied.
fragment float4 fg_fragment_color(
    GVOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]],
    sampler atlas_sampler [[sampler(0)]]
) {
    float4 texel = atlas.sample(atlas_sampler, in.uv);
    return texel * in.color.a;
}

// ----------------------------------------------------------------------
// Dot pass — circle-clipped coloured quad.  Reuses the BG vertex
// shader for placement; the fragment discards pixels outside the
// inscribed circle and antialiases a 1-px ring at the edge.  Used
// for the sidebar status dots that the AppKit renderer draws via
// `fill_ellipse_in_rect`.
// ----------------------------------------------------------------------

struct DVOut {
    float4 position [[position]];
    float2 quad_uv;   // 0..1 across the source quad
    float4 color;
};

vertex DVOut dot_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    device const Cell* cells [[buffer(0)]],
    constant float2& viewport_px [[buffer(1)]]
) {
    Cell c = cells[iid];
    float2 px = c.origin + c.size * corners[vid];
    float2 ndc = (px / viewport_px) * 2.0 - 1.0;
    ndc.y = -ndc.y;

    DVOut o;
    o.position = float4(ndc, 0.0, 1.0);
    o.quad_uv = corners[vid];
    o.color = c.color;
    return o;
}

fragment float4 dot_fragment(DVOut in [[stage_in]]) {
    float d = distance(in.quad_uv, float2(0.5, 0.5));
    // Hard radius 0.5; antialias band 1 / quad-size-in-px.  We don't
    // know the quad's exact pixel size in the shader without a
    // uniform, but the dot is tiny (9 px) so a fixed `fwidth`-style
    // gradient gives an acceptable single-pixel soft edge on Retina.
    float aa = fwidth(d);
    float coverage = 1.0 - smoothstep(0.5 - aa, 0.5, d);
    return float4(in.color.rgb, in.color.a * coverage);
}

// ----------------------------------------------------------------------
// UI rect pass — anti-aliased rounded rectangle via SDF.  Each
// instance is a panel / chrome element (search bar, future menu,
// future tooltip) that needs:
//   • Pixel-precise positioning (not cell-aligned)
//   • A real fillet corner radius (no Unicode box-drawing approx)
//   • Optional 1-px stroke border
//   • Optional soft drop shadow (alpha falloff outside the rect)
// All computed in one fragment shader so a single instance pushes
// BG + border + shadow without extra pipeline switches.
// ----------------------------------------------------------------------

struct UiRect {
    float2 origin;        // top-left in physical px (the FILL rect)
    float2 size;          // fill rect size in physical px
    float4 fill_color;    // panel BG, premultiplied alpha allowed
    float4 border_color;  // 1-px stroke; alpha=0 to skip
    float corner_radius;  // px; clamped to min(size)/2 in shader
    float border_width;   // px; 0 to skip border
    float shadow_blur;    // px; 0 to skip shadow
    float shadow_alpha;   // 0..1 shadow intensity
    float4 shadow_color;  // typically black, alpha-mixed by shadow_alpha
};

struct URVOut {
    float4 position [[position]];
    float2 quad_uv;       // 0..1 across the *padded* (shadow-inclusive) quad
    // Per-instance constants (forwarded to fragment so each pixel can SDF).
    float2 padded_size;   // size + 2 * shadow_blur, mirrored from vertex
    float2 fill_size;     // size, mirrored from vertex (no inflation)
    float4 fill_color;
    float4 border_color;
    float corner_radius;
    float border_width;
    float shadow_blur;
    float shadow_alpha;
    float4 shadow_color;
};

vertex URVOut ui_rect_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    device const UiRect* rects [[buffer(0)]],
    constant float2& viewport_px [[buffer(1)]]
) {
    UiRect r = rects[iid];
    // Inflate the quad by shadow_blur on each side so the shadow falloff
    // has pixels to draw into.  Drop shadow becomes a 0-cost optional
    // by setting shadow_blur=0.
    float2 padded_origin = r.origin - float2(r.shadow_blur, r.shadow_blur);
    float2 padded_size = r.size + float2(2.0 * r.shadow_blur, 2.0 * r.shadow_blur);
    float2 px = padded_origin + padded_size * corners[vid];

    float2 ndc = (px / viewport_px) * 2.0 - 1.0;
    ndc.y = -ndc.y;

    URVOut o;
    o.position = float4(ndc, 0.0, 1.0);
    o.quad_uv = corners[vid];
    o.padded_size = padded_size;
    o.fill_size = r.size;
    o.fill_color = r.fill_color;
    o.border_color = r.border_color;
    o.corner_radius = r.corner_radius;
    o.border_width = r.border_width;
    o.shadow_blur = r.shadow_blur;
    o.shadow_alpha = r.shadow_alpha;
    o.shadow_color = r.shadow_color;
    return o;
}

fragment float4 ui_rect_fragment(URVOut in [[stage_in]]) {
    // Convert quad_uv (0..1 across padded quad) back to a centred
    // coord in fill-rect space (i.e. the SDF reference frame is the
    // fill rect, not the padded shadow quad).
    float2 p = (in.quad_uv * in.padded_size) - in.padded_size * 0.5;
    // Clamp corner radius defensively.
    float radius = min(in.corner_radius, min(in.fill_size.x, in.fill_size.y) * 0.5);
    float2 fill_half_ext = in.fill_size * 0.5;
    // Inline SDF for axis-aligned rounded rect — avoids referencing a
    // free function (Metal had trouble linking ours; inlining sidesteps
    // any linker quirk).
    float2 q = abs(p) - fill_half_ext + float2(radius, radius);
    float d = min(max(q.x, q.y), 0.0) + length(max(q, float2(0.0, 0.0))) - radius;

    // Anti-alias band: ~1 px in screen space.
    float aa = fwidth(d) * 0.7;

    // Inside-vs-edge coverage for the fill.
    float fill_coverage = 1.0 - smoothstep(-aa, aa, d);

    // Inside-stroke border: the border lives in `-border_width < d < 0`,
    // i.e. the outermost `border_width` pixels of the rect's INTERIOR.
    // This matches the CSS `box-sizing: border-box` convention — a 1 px
    // border eats 1 px of interior, it never extends outside the rect.
    // (The old `|d| < border_width/2` "centered stroke" extended half a
    // pixel past the rect bounds, which subtly inflated every card by
    // ~1 px and was the source of the "border looks clipped" feedback.)
    float border_coverage = 0.0;
    if (in.border_width > 0.0 && in.border_color.a > 0.0) {
        float outside_band = 1.0 - smoothstep(-aa, aa, d);
        float beyond_inner = smoothstep(-aa, aa, d + in.border_width);
        border_coverage = outside_band * beyond_inner;
    }

    // Soft shadow falloff outside the fill rect.
    float shadow_coverage = 0.0;
    if (in.shadow_blur > 0.0 && in.shadow_alpha > 0.0) {
        // d > 0 outside; ramp from full at d=0 to 0 at d=shadow_blur.
        shadow_coverage = (1.0 - smoothstep(0.0, in.shadow_blur, max(d, 0.0))) * in.shadow_alpha;
        // Don't draw shadow inside the rect — fill takes over there.
        shadow_coverage *= step(0.0, d);
    }

    // Composite: shadow underneath, fill on top, border on top of both.
    // Premultiplied, like the fill and border layers below — the
    // pipeline blends this shader's output with a `One` source factor,
    // so every layer must arrive pre-scaled.  This line used to pass
    // `shadow_color.rgb` unscaled, which was invisible only because
    // every caller happens to use a black shadow (black × anything is
    // still black).  A coloured shadow would have rendered at full
    // brightness regardless of its alpha.
    float shadow_a = in.shadow_color.a * shadow_coverage;
    float4 rgba = float4(in.shadow_color.rgb * shadow_a, shadow_a);
    // Fill over shadow.
    {
        float a = in.fill_color.a * fill_coverage;
        float inv = 1.0 - a;
        rgba.rgb = in.fill_color.rgb * a + rgba.rgb * inv;
        rgba.a   = a + rgba.a * inv;
    }
    // Border over fill.
    if (border_coverage > 0.0) {
        float a = in.border_color.a * border_coverage;
        float inv = 1.0 - a;
        rgba.rgb = in.border_color.rgb * a + rgba.rgb * inv;
        rgba.a   = a + rgba.a * inv;
    }
    return rgba;
}
