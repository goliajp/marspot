//! Metal source for the copying presenter's quad.

pub(crate) const SHADER_SRC: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float2 uv;
};

// Texture-to-drawable size ratio: the quad's bottom-right corner
// lands at the texture's natural pixel size inside the drawable,
// anchored top-left.  Stale IOSurface during a window resize stays at
// its native pixel size; the drawable's exposed band is filled with
// the terminal's background (set as `clearColor`), which is the *exact* same
// colour as the terminal cell BG — so the seam between the
// IOSurface region and the fill is invisible to the eye.
struct ViewParams {
    float ratio_x; // texture_w / drawable_w
    float ratio_y; // texture_h / drawable_h
};

vertex VertexOut shell_quad_vs(uint vid [[vertex_id]],
                               constant ViewParams& vp [[buffer(0)]]) {
    const float2 uvs[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 uv = uvs[vid];
    float ndc_x = uv.x == 0.0 ? -1.0 : (-1.0 + 2.0 * vp.ratio_x);
    float ndc_y = uv.y == 0.0 ?  1.0 : ( 1.0 - 2.0 * vp.ratio_y);
    VertexOut out;
    out.position = float4(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

fragment float4 shell_quad_fs(VertexOut in [[stage_in]],
                              texture2d<float> tex [[texture(0)]],
                              sampler smp [[sampler(0)]]) {
    return tex.sample(smp, in.uv);
}
"#;
