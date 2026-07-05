//! Translate a [`TerrainGraph`] into a WGSL compute shader that evaluates the
//! graph over a grid of world columns, writing height + structure-density
//! buffers. Node parameters live in a `P[]` storage buffer (indexed by a stable
//! per-node base offset), so dragging a slider only rewrites `P` — the shader
//! is regenerated/recompiled only when the graph *structure* changes.
//!
//! Every node becomes a WGSL `fn node_i(x, z) -> f32`, emitted in topological
//! order (children before callers, since WGSL has no forward references).
//! `DomainWarp` simply calls its input with shifted coordinates — the same
//! coordinate-parameterized model the CPU oracle uses.
//!
//! Noise nodes use a standard Ashima/Gustavson simplex (`snoise`), which is
//! character-equivalent to — but not bit-identical with — the `noise` crate the
//! game/CPU-preview use (mirroring the existing JS-vs-Rust simplex note). The
//! 2D CPU preview remains the exact game-noise ground truth; this GPU pass is
//! the fast interactive 3D view. Non-noise nodes match the CPU oracle exactly,
//! which is what `tests/gpu_codegen.rs` asserts.

use std::fmt::Write;

use soils_worldgen::graph::{Axis, In, NodeKind, TerrainGraph};

/// The tunable f32 parameters of a node, in the fixed order the shader indexes.
/// Structural choices (node kind, `Coord` axis, `Fbm` octaves, wiring) are NOT
/// here — changing them regenerates the shader.
pub fn node_params(kind: &NodeKind) -> Vec<f32> {
    match *kind {
        NodeKind::Constant { value } => vec![value],
        NodeKind::Coord { .. } => vec![],
        NodeKind::Simplex2 { frequency, offset } => vec![frequency, offset[0], offset[1]],
        NodeKind::Fbm { base_frequency, lacunarity, persistence, offset, .. } => {
            vec![base_frequency, lacunarity, persistence, offset[0], offset[1]]
        }
        NodeKind::RadialFalloff { center, radius, exponent } => {
            vec![center[0], center[1], radius, exponent]
        }
        NodeKind::Abs { .. } => vec![],
        NodeKind::ScaleBias { scale, bias, .. } => vec![scale, bias],
        NodeKind::Clamp { min, max, .. } => vec![min, max],
        NodeKind::Power { exponent, .. } => vec![exponent],
        NodeKind::Terrace { steps, .. } => vec![steps],
        NodeKind::Add { .. }
        | NodeKind::Sub { .. }
        | NodeKind::Mul { .. }
        | NodeKind::Min { .. }
        | NodeKind::Max { .. }
        | NodeKind::Lerp { .. } => vec![],
        NodeKind::DomainWarp { amount, .. } => vec![amount],
    }
}

/// The full `P[]` vector for a graph (nodes in index order). Aligns 1:1 with the
/// base offsets [`generate`] bakes into the shader.
pub fn collect_params(graph: &TerrainGraph) -> Vec<f32> {
    let mut p = Vec::new();
    for node in &graph.nodes {
        p.extend(node_params(&node.kind));
    }
    p
}

/// Per-node base offsets into `P[]` (index order).
fn param_bases(graph: &TerrainGraph) -> Vec<usize> {
    let mut base = vec![0usize; graph.nodes.len()];
    let mut acc = 0usize;
    for (i, node) in graph.nodes.iter().enumerate() {
        base[i] = acc;
        acc += node_params(&node.kind).len();
    }
    base
}

/// Emit all node functions (topological order) plus `height_out`/`structure_out`.
/// Shared by the compute shader and the preview material — both reference the
/// same `P[]` param array and `snoise2`.
fn emit_functions(graph: &TerrainGraph) -> String {
    let base = param_bases(graph);
    let mut s = String::new();
    for i in topo_order(graph) {
        emit_node(&mut s, graph, i, base[i]);
    }
    let height = emit_in(&graph.outputs.height, "x", "z");
    let structure =
        graph.outputs.structure.as_ref().map_or("0.0".to_string(), |o| emit_in(o, "x", "z"));
    let _ = writeln!(s, "fn height_out(x: f32, z: f32) -> f32 {{ return {height}; }}");
    let _ = writeln!(s, "fn structure_out(x: f32, z: f32) -> f32 {{ return {structure}; }}\n");
    s
}

/// Generate the compute shader source for `graph` (used by the headless parity
/// test and available for a compute-driven preview). The live preview uses
/// [`generate_material`]; this compute variant is what `tests/gpu_codegen.rs`
/// validates against the CPU oracle.
#[allow(dead_code)]
pub fn generate(graph: &TerrainGraph) -> String {
    let mut s = String::new();
    s.push_str(NOISE_PRELUDE);
    s.push_str("struct View { p: vec4<f32> };\n");
    s.push_str("@group(0) @binding(0) var<uniform> view: View;\n");
    s.push_str("@group(0) @binding(1) var<storage, read> P: array<f32>;\n");
    s.push_str("@group(0) @binding(2) var<storage, read_write> out_height: array<f32>;\n");
    s.push_str("@group(0) @binding(3) var<storage, read_write> out_structure: array<f32>;\n\n");
    s.push_str(&emit_functions(graph));
    s.push_str(COMPUTE_MAIN);
    s
}

/// Generate a Bevy `Material` shader (vertex + fragment) that evaluates the
/// graph directly: the vertex shader displaces a grid by `height_out(x,z)` and
/// computes a finite-difference normal; the fragment shades it. This drives the
/// live 3D preview (the compute variant is validated separately by the test).
#[allow(dead_code)] // used by the bin (preview3d), not by the gpu_codegen test include
pub fn generate_material(graph: &TerrainGraph) -> String {
    let mut s = String::new();
    s.push_str(NOISE_PRELUDE);
    s.push_str(MATERIAL_HEADER);
    s.push_str(&emit_functions(graph));
    s.push_str(MATERIAL_BODY);
    s
}

/// Emit `fn node_i(x, z) -> f32 { ... }`.
fn emit_node(s: &mut String, graph: &TerrainGraph, i: usize, b: usize) {
    let _ = write!(s, "fn node_{i}(x: f32, z: f32) -> f32 {{ ");
    let body = match &graph.nodes[i].kind {
        NodeKind::Constant { .. } => format!("return P[{b}];"),
        NodeKind::Coord { axis } => match axis {
            Axis::X => "return x;".to_string(),
            Axis::Z => "return z;".to_string(),
        },
        NodeKind::Simplex2 { .. } => {
            format!("return snoise2(vec2<f32>(x * P[{b}] + P[{}], z * P[{b}] + P[{}]));", b + 1, b + 2)
        }
        NodeKind::Fbm { octaves, .. } => format!(
            "var f = P[{b}]; var amp = 1.0; var acc = 0.0; \
             for (var o = 0u; o < {octaves}u; o = o + 1u) {{ \
             acc = acc + amp * snoise2(vec2<f32>(x * f + P[{}], z * f + P[{}])); \
             f = f * P[{}]; amp = amp * P[{}]; }} return acc;",
            b + 3,
            b + 4,
            b + 1,
            b + 2,
        ),
        NodeKind::RadialFalloff { .. } => format!(
            "let dx = x - P[{b}]; let dz = z - P[{}]; \
             let d = sqrt(dx * dx + dz * dz) / max(P[{}], 1e-6); \
             return pow(1.0 - clamp(d, 0.0, 1.0), P[{}]);",
            b + 1,
            b + 2,
            b + 3,
        ),
        NodeKind::Abs { input } => format!("return abs({});", emit_in(input, "x", "z")),
        NodeKind::ScaleBias { input, .. } => {
            format!("return {} * P[{b}] + P[{}];", emit_in(input, "x", "z"), b + 1)
        }
        NodeKind::Clamp { input, .. } => {
            format!("return clamp({}, P[{b}], P[{}]);", emit_in(input, "x", "z"), b + 1)
        }
        NodeKind::Power { input, .. } => format!("return pow({}, P[{b}]);", emit_in(input, "x", "z")),
        NodeKind::Terrace { input, .. } => {
            format!("let s = max(P[{b}], 1.0); return round({} * s) / s;", emit_in(input, "x", "z"))
        }
        NodeKind::Add { a, b: bb } => bin("+", a, bb),
        NodeKind::Sub { a, b: bb } => bin("-", a, bb),
        NodeKind::Mul { a, b: bb } => bin("*", a, bb),
        NodeKind::Min { a, b: bb } => {
            format!("return min({}, {});", emit_in(a, "x", "z"), emit_in(bb, "x", "z"))
        }
        NodeKind::Max { a, b: bb } => {
            format!("return max({}, {});", emit_in(a, "x", "z"), emit_in(bb, "x", "z"))
        }
        NodeKind::Lerp { a, b: bb, t } => format!(
            "let va = {}; let vb = {}; let tt = clamp({}, 0.0, 1.0); return va + (vb - va) * tt;",
            emit_in(a, "x", "z"),
            emit_in(bb, "x", "z"),
            emit_in(t, "x", "z"),
        ),
        NodeKind::DomainWarp { input, wx, wz, .. } => format!(
            "let nx = x + {} * P[{b}]; let nz = z + {} * P[{b}]; return {};",
            emit_in(wx, "x", "z"),
            emit_in(wz, "x", "z"),
            emit_in(input, "nx", "nz"),
        ),
    };
    s.push_str(&body);
    s.push_str(" }\n");
}

fn bin(op: &str, a: &In, b: &In) -> String {
    format!("return {} {op} {};", emit_in(a, "x", "z"), emit_in(b, "x", "z"))
}

/// A wired input becomes a call to its node function at `(xvar, zvar)`; an
/// unwired input is its literal default.
fn emit_in(slot: &In, xvar: &str, zvar: &str) -> String {
    match slot.node {
        Some(j) => format!("node_{j}({xvar}, {zvar})"),
        None => wgsl_f32(slot.default),
    }
}

/// Format an f32 as a valid WGSL float literal.
fn wgsl_f32(x: f32) -> String {
    if x.is_nan() {
        "0.0".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "3.0e38".to_string() } else { "-3.0e38".to_string() }
    } else if x == x.trunc() && x.abs() < 1e15 {
        format!("{:.1}", x)
    } else {
        format!("{x:?}")
    }
}

/// Topological order (children first) via iterative post-order DFS. The graph is
/// acyclic (checked when lowering), so this terminates.
fn topo_order(graph: &TerrainGraph) -> Vec<usize> {
    let n = graph.nodes.len();
    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        while let Some(&(node, child)) = stack.last() {
            let inputs: Vec<usize> =
                graph.nodes[node].kind.inputs().into_iter().filter_map(|s| s.node).collect();
            if child == 0 {
                visited[node] = true;
            }
            if child < inputs.len() {
                stack.last_mut().unwrap().1 += 1;
                let next = inputs[child];
                if !visited[next] {
                    stack.push((next, 0));
                }
            } else {
                order.push(node);
                stack.pop();
            }
        }
    }
    order
}

/// Simplex prelude. `snoise2` is the standard Ashima Arts / Stefan Gustavson
/// WGSL simplex (public domain).
const NOISE_PRELUDE: &str = r#"
fn mod289_3(x: vec3<f32>) -> vec3<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn mod289_2(x: vec2<f32>) -> vec2<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn permute3(x: vec3<f32>) -> vec3<f32> { return mod289_3(((x * 34.0) + 1.0) * x); }

fn snoise2(v: vec2<f32>) -> f32 {
    let C = vec4<f32>(0.211324865405187, 0.366025403784439, -0.577350269189626, 0.024390243902439);
    var i = floor(v + dot(v, C.yy));
    let x0 = v - i + dot(i, C.xx);
    var i1 = vec2<f32>(0.0, 0.0);
    if (x0.x > x0.y) { i1 = vec2<f32>(1.0, 0.0); } else { i1 = vec2<f32>(0.0, 1.0); }
    var x12 = x0.xyxy + C.xxzz;
    x12 = vec4<f32>(x12.xy - i1, x12.zw);
    i = mod289_2(i);
    let p = permute3(permute3(i.y + vec3<f32>(0.0, i1.y, 1.0)) + i.x + vec3<f32>(0.0, i1.x, 1.0));
    var m = max(0.5 - vec3<f32>(dot(x0, x0), dot(x12.xy, x12.xy), dot(x12.zw, x12.zw)), vec3<f32>(0.0));
    m = m * m; m = m * m;
    let x = 2.0 * fract(p * C.www) - 1.0;
    let h = abs(x) - 0.5;
    let ox = floor(x + 0.5);
    let a0 = x - ox;
    m = m * (1.79284291400159 - 0.85373472095314 * (a0 * a0 + h * h));
    var g = vec3<f32>(0.0, 0.0, 0.0);
    g.x = a0.x * x0.x + h.x * x0.y;
    let gyz = a0.yz * x12.xz + h.yz * x12.yw;
    g.y = gyz.x; g.z = gyz.y;
    return 130.0 * dot(m, g);
}
"#;

/// The compute entry point. Fixed regardless of graph. Used by [`generate`].
#[allow(dead_code)]
const COMPUTE_MAIN: &str = r#"
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let res = u32(view.p.w);
    if (gid.x >= res || gid.y >= res) { return; }
    let x = view.p.x + f32(gid.x) * view.p.z;
    let z = view.p.y + f32(gid.y) * view.p.z;
    let idx = gid.y * res + gid.x;
    out_height[idx] = height_out(x, z);
    out_structure[idx] = structure_out(x, z);
}
"#;

/// Bindings + imports for the preview material. `pv.a = (res, origin, step,
/// hscale)`, `pv.b = (hmin, hmax, _, _)`.
#[allow(dead_code)] // used via generate_material (bin only)
const MATERIAL_HEADER: &str = r#"
#import bevy_pbr::{mesh_view_bindings::view, view_transformations::position_world_to_clip}

struct Pv { a: vec4<f32>, b: vec4<f32> };
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> P: array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> pv: Pv;

"#;

/// Vertex (displace a grid by `height_out`) + fragment (colour ramp + lambert).
#[allow(dead_code)] // used via generate_material (bin only)
const MATERIAL_BODY: &str = r#"
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) n: vec3<f32>,
    @location(1) h: f32,
};

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
    let res = u32(pv.a.x);
    let origin = pv.a.y;
    let step = pv.a.z;
    let hscale = pv.a.w;
    let hmin = pv.b.x;
    let gx = vi % res;
    let gz = vi / res;
    let x = origin + f32(gx) * step;
    let z = origin + f32(gz) * step;
    let h = height_out(x, z);
    let hx = height_out(x + step, z) - height_out(x - step, z);
    let hz = height_out(x, z + step) - height_out(x, z - step);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_align_with_default_graph() {
        let g = TerrainGraph::default_soils();
        let p = collect_params(&g);
        // Sum of per-node param counts equals the flat vector length.
        let total: usize = g.nodes.iter().map(|n| node_params(&n.kind).len()).sum();
        assert_eq!(p.len(), total);
        // The shader references at most P[total-1].
        let src = generate(&g);
        assert!(src.contains("fn main"));
        assert!(src.contains("height_out"));
    }

    #[test]
    fn topo_order_places_children_first() {
        let g = TerrainGraph::default_soils();
        let order = topo_order(&g);
        assert_eq!(order.len(), g.nodes.len());
        let pos: std::collections::HashMap<usize, usize> =
            order.iter().enumerate().map(|(p, &i)| (i, p)).collect();
        for node in &g.nodes {
            for input in node.kind.inputs() {
                if let Some(src) = input.node {
                    assert!(pos[&src] < pos[&node.id], "child {src} must precede {}", node.id);
                }
            }
        }
    }
}
