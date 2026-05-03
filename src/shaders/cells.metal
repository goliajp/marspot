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
