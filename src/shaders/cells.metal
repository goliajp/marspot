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
