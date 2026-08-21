//! Terrain-preview material shader: wraps the shared fixed-point codegen
//! (`soils_worldgen::wgsl`) with a Bevy `Material` vertex/fragment pair. The
//! vertex shader displaces a grid by `height_out(x,z)` (converted from Q16.16
//! for display) and computes a finite-difference normal; the fragment shades
//! it. The node math is the *same* bit-exact fx pipeline the game generates
//! chunks with — the preview shows exactly what will generate.

use soils_worldgen::TerrainGraph;
use soils_worldgen::wgsl;

pub use soils_worldgen::wgsl::collect_params;

/// Generate the preview material shader. Uncompilable graphs (unsupported
/// node kinds mid-edit) get a flat fallback so the tool never crashes.
pub fn generate_material(graph: &TerrainGraph) -> String {
    let mut s = String::new();
    s.push_str(MATERIAL_HEADER);
    match wgsl::emit_functions(graph) {
        Ok(fns) => s.push_str(&fns),
        Err(_) => s.push_str(FALLBACK_FNS),
    }
    s.push_str(MATERIAL_BODY);
    s
}

/// Bindings + imports for the preview material. `pv.a = (res, origin, step,
/// hscale)`, `pv.b = (hmin, hmax, seed_bits, _)`.
const MATERIAL_HEADER: &str = r#"
#import bevy_pbr::{mesh_view_bindings::view, view_transformations::position_world_to_clip}

struct Pv { a: vec4<f32>, b: vec4<f32> };
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> P: array<i32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> pv: Pv;

var<private> SEED: u32;

"#;

/// Flat stand-in when the graph doesn't compile (e.g. an unsupported node was
/// just dropped on the canvas).
const FALLBACK_FNS: &str = r#"
fn height_out(x: i32, z: i32) -> i32 { return 0; }
fn rock_out(x: i32, z: i32) -> i32 { return 0; }
fn structure_out(x: i32, z: i32) -> i32 { return 0; }
"#;

/// Vertex (displace a grid by `height_out`) + fragment (colour ramp + lambert).
const MATERIAL_BODY: &str = r#"
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) n: vec3<f32>,
    @location(1) h: f32,
};

// Display-only conversion: f32 preview coordinate -> Q16.16 -> f32 height.
fn hval(x: f32, z: f32) -> f32 {
    return f32(height_out(i32(round(x * 65536.0)), i32(round(z * 65536.0)))) / 65536.0;
}

fn ramp(t: f32) -> vec3<f32> {
    let c0 = vec3<f32>(0.12, 0.24, 0.47);
    let c1 = vec3<f32>(0.78, 0.74, 0.51);
    let c2 = vec3<f32>(0.27, 0.55, 0.24);
    let c3 = vec3<f32>(0.43, 0.39, 0.35);
    let c4 = vec3<f32>(0.94, 0.94, 0.96);
    let u = clamp(t, 0.0, 1.0);
    if (u < 0.4)  { return mix(c0, c1, u / 0.4); }
    if (u < 0.55) { return mix(c1, c2, (u - 0.4) / 0.15); }
    if (u < 0.8)  { return mix(c2, c3, (u - 0.55) / 0.25); }
    return mix(c3, c4, (u - 0.8) / 0.2);
}

@vertex
fn vertex(@builtin(vertex_index) vi: u32) -> VOut {
    SEED = bitcast<u32>(pv.b.z);
    let res = u32(pv.a.x);
    let origin = pv.a.y;
    let step = pv.a.z;
    let hscale = pv.a.w;
    let hmin = pv.b.x;
    let gx = vi % res;
    let gz = vi / res;
    let x = origin + f32(gx) * step;
    let z = origin + f32(gz) * step;
    let h = hval(x, z);
    let hx = hval(x + step, z) - hval(x - step, z);
    let hz = hval(x, z + step) - hval(x, z - step);
    let n = normalize(vec3<f32>(-hx * hscale, 2.0 * step, -hz * hscale));
    let wp = vec3<f32>(x, (h - hmin) * hscale, z);
    var out: VOut;
    out.clip = position_world_to_clip(wp);
    out.n = n;
    out.h = h;
    return out;
}

@fragment
fn fragment(in: VOut) -> @location(0) vec4<f32> {
    let t = (in.h - pv.b.x) / max(pv.b.y - pv.b.x, 1e-3);
    let base = ramp(t);
    let l = clamp(dot(normalize(in.n), normalize(vec3<f32>(0.5, 1.0, 0.35))), 0.0, 1.0) * 0.8 + 0.25;
    return vec4<f32>(base * l, 1.0);
}
"#;
