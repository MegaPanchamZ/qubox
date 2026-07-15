// WGSL tone-mapping shader. Composed into `render_wgpu.rs` via
// `include_str!`.
//
// Reference sources:
//   * Hable filmic: John Hable, "Uncharted 2: HDR Lighting",
//     GDC 2010. Curve constants match the public-domain
//     `filmic_curve` snippet.
//   * BT.2390 perceptual quantizer (PQ): ITU-R BT.2390 §3,
//     approximation used by libplacebo and Chromium.

const HABLE_A: f32 = 0.15;
const HABLE_B: f32 = 0.50;
const HABLE_C: f32 = 0.10;
const HABLE_D: f32 = 0.20;
const HABLE_E: f32 = 0.02;
const HABLE_F: f32 = 0.30;
const HABLE_W: f32 = 11.2;

// Numerically stable Hable curve — assumes input in linear HDR
// (relative units, ≥ 0). Output is in [0, 1] suitable for sRGB
// encoding downstream.
fn hable_curve(x: f32) -> f32 {
    let a = HABLE_A * x - HABLE_B * x;
    let b = HABLE_C * x + HABLE_D * x;
    let num = (a * x + HABLE_E);
    let denom = (b * x + HABLE_F);
    let denom_clamped = max(denom, 1e-5);
    let curve = num / denom_clamped;
    let linear = ((HABLE_W * x) + 0.004) / (HABLE_W + 0.004);
    return curve - linear;
}

// Hable filmic operator applied per-channel.
fn hable_tone_map(color: vec3<f32>) -> vec3<f32> {
    let exposure_bias = 2.0;
    let curr = hable_curve(color * exposure_bias);
    let white_scale = vec3<f32>(1.0) / hable_curve(vec3<f32>(HABLE_W));
    return curr * white_scale;
}

// BT.2390 perceptual quantizer (PQ). Input is linear light in
// 0..10_000 cd/m² range; output is 0..1 PQ-domain. Caller is
// responsible for the nominal 1 000 cd/m² whitepoint scaling via
// the `white_point` parameter.
fn pq_curve(x: f32, white_point: f32) -> f32 {
    let scaled = max(x, 0.0) / max(white_point, 1.0);
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let num = m1 * scaled + c1;
    let denom = m2 * scaled + c2;
    let t = clamp(num / max(denom, 1e-9), 0.0, 1.0);
    return clamp(t, 0.0, 1.0);
}

// BT.2390 perceptual quantizer applied per-channel. The whitepoint
// parameter encodes the peak luminance in cd/m² (1_000 is the
// canonical HDR10 reference white).
fn bt2390_tone_map(color: vec3<f32>) -> vec3<f32> {
    let white_point = 1000.0;
    return vec3<f32>(
        pq_curve(color.r, white_point),
        pq_curve(color.g, white_point),
        pq_curve(color.b, white_point),
    );
}

// sRGB passthrough (no tone mapping) — used when the source is
// already display-referred and the user wants a one-to-one preview.
fn passthrough_tone_map(color: vec3<f32>) -> vec3<f32> {
    return clamp(color, vec3<f32>(0.0), vec3<f32>(1.0));
}

@vertex
fn vs(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let pos = positions[idx];
    return vec4<f32>(pos, 0.0, 1.0);
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_passthrough(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex));
    let uv = (pos.xy + vec2<f32>(0.5)) / dims;
    var color = textureSample(tex, samp, uv);
    if (color.a < 1.0) {
        color = vec4<f32>(1.0, 1.0, 1.0, 1.0);
    }
    return color;
}

@fragment
fn fs_hable(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex));
    let uv = (pos.xy + vec2<f32>(0.5)) / dims;
    var color = textureSample(tex, samp, uv).rgb;
    color = hable_tone_map(color);
    return vec4<f32>(color, 1.0);
}

@fragment
fn fs_bt2390(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex));
    let uv = (pos.xy + vec2<f32>(0.5)) / dims;
    var color = textureSample(tex, samp, uv).rgb;
    color = bt2390_tone_map(color);
    return vec4<f32>(color, 1.0);
}
