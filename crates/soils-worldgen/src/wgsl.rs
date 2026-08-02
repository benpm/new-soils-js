//! Translate a [`TerrainGraph`] into fixed-point WGSL that is **bit-exact**
//! with the CPU evaluator ([`crate::graph::CompiledGraph`]) — noise included.
//! Every emitted operation maps 1:1 onto an `fx_`/`dn_` function from
//! [`crate::noise_det::WGSL_PRELUDE`], which mirrors [`crate::fx`] /
//! [`crate::noise_det`] exactly; `tests/gpu_codegen.rs` (terrainlab harness)
//! asserts equality entry-for-entry against `columns_fx`.
//!
//! Node parameters live in a `P[]` storage buffer of Q16.16 `i32`s (indexed by
//! a stable per-node base offset), so dragging a slider only rewrites `P` —
//! the shader is regenerated/recompiled only when the graph *structure*
//! changes. Parameters are quantized with the same `fx::from_f32` the CPU
//! compile uses, so the two ends consume identical integers.

use std::fmt::Write;

use crate::fx;
use crate::graph::{Axis, In, NodeKind, TerrainGraph};
use crate::noise_det::WGSL_PRELUDE;

/// The tunable parameters of a node as Q16.16, in the fixed order the shader
/// indexes. Structural choices (node kind, `Coord` axis, `Fbm` octaves,
/// wiring) are NOT here — changing them regenerates the shader.
pub fn node_params(kind: &NodeKind) -> Vec<i32> {
    let q = fx::from_f32;
    match *kind {
        NodeKind::Constant { value } => vec![q(value)],
        NodeKind::Coord { .. } => vec![],
        NodeKind::Simplex2 { frequency, offset } => {
            vec![q(frequency), q(offset[0]), q(offset[1])]
        }
        NodeKind::Fbm { base_frequency, lacunarity, persistence, offset, .. } => {
            vec![q(base_frequency), q(lacunarity), q(persistence), q(offset[0]), q(offset[1])]
        }
        NodeKind::Abs { .. } => vec![],
        NodeKind::ScaleBias { scale, bias, .. } => vec![q(scale), q(bias)],
        NodeKind::Clamp { min, max, .. } => vec![q(min), q(max)],
        NodeKind::Terrace { steps, .. } => {
            // (steps, 1/steps) — the same precomputed inverse as CPU compile.
            let s = steps.max(1.0);
            vec![q(s), (65536.0 / s as f64).round() as i32]
        }
        NodeKind::Add { .. }
        | NodeKind::Sub { .. }
        | NodeKind::Mul { .. }
        | NodeKind::Min { .. }
        | NodeKind::Max { .. }
        | NodeKind::Lerp { .. } => vec![],
        NodeKind::DomainWarp { amount, .. } => vec![q(amount)],
        // Rejected by validate(); no param layout.
        NodeKind::Power { .. } | NodeKind::RadialFalloff { .. } => vec![],
    }
}

/// The full `P[]` vector for a graph (nodes in index order). Aligns 1:1 with
/// the base offsets [`emit_functions`] bakes into the shader.
pub fn collect_params(graph: &TerrainGraph) -> Vec<i32> {
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

/// Emit the noise prelude, all node functions (topological order), plus
/// `height_out`/`structure_out`/`rock_out`, all `fn(i32, i32) -> i32` over
/// Q16.16 coordinates. The caller supplies bindings for `P: array<i32>` and a
/// `SEED: u32` override constant (or `const SEED` before this block).
/// Errors on graphs [`TerrainGraph::validate`] rejects.
pub fn emit_functions(graph: &TerrainGraph) -> Result<String, String> {
    graph.validate()?;
    let base = param_bases(graph);
    let mut s = String::new();
    s.push_str(WGSL_PRELUDE);
    for i in topo_order(graph) {
        emit_node(&mut s, graph, i, base[i]);
    }
    let height = emit_in(&graph.outputs.height, "x", "z");
    let rock = graph.outputs.rock.as_ref().map_or("0".to_string(), |o| emit_in(o, "x", "z"));
    let structure =
        graph.outputs.structure.as_ref().map_or("0".to_string(), |o| emit_in(o, "x", "z"));
    let _ = writeln!(s, "fn height_out(x: i32, z: i32) -> i32 {{ return {height}; }}");
    let _ = writeln!(s, "fn rock_out(x: i32, z: i32) -> i32 {{ return {rock}; }}");
    let _ = writeln!(s, "fn structure_out(x: i32, z: i32) -> i32 {{ return {structure}; }}\n");
    Ok(s)
}

/// Generate the column-evaluation compute shader (the GPU-parity test kernel):
/// a `res`×`res` grid of world columns starting at `origin` with integer-voxel
/// `step`, writing Q16.16 height/rock/structure buffers.
pub fn generate_columns(graph: &TerrainGraph) -> Result<String, String> {
    let mut s = String::new();
    s.push_str(COLUMNS_HEADER);
    s.push_str(&emit_functions(graph)?);
    s.push_str(COLUMNS_MAIN);
    Ok(s)
}

/// Emit `fn node_i(x: i32, z: i32) -> i32`. Bodies mirror
/// `CompiledGraph::eval_node` arm-for-arm.
fn emit_node(s: &mut String, graph: &TerrainGraph, i: usize, b: usize) {
    let _ = write!(s, "fn node_{i}(x: i32, z: i32) -> i32 {{ ");
    let body = match &graph.nodes[i].kind {
        NodeKind::Constant { .. } => format!("return P[{b}];"),
        NodeKind::Coord { axis } => match axis {
            Axis::X => "return x;".to_string(),
            Axis::Z => "return z;".to_string(),
        },
        NodeKind::Simplex2 { .. } => format!(
            "return dn_noise2(SEED, fx_mul(x, P[{b}]) + P[{}], fx_mul(z, P[{b}]) + P[{}]);",
            b + 1,
            b + 2
        ),
        NodeKind::Fbm { octaves, .. } => format!(
            "var f = P[{b}]; var amp = FX_ONE; var acc = 0; \
             for (var o = 0u; o < {octaves}u; o = o + 1u) {{ \
             let n = dn_noise2(SEED, fx_mul(x, f) + P[{}], fx_mul(z, f) + P[{}]); \
             acc = acc + fx_mul(amp, n); \
             f = fx_mul(f, P[{}]); amp = fx_mul(amp, P[{}]); }} return acc;",
            b + 3,
            b + 4,
            b + 1,
            b + 2,
        ),
        NodeKind::Abs { input } => format!("return fx_abs({});", emit_in(input, "x", "z")),
        NodeKind::ScaleBias { input, .. } => {
            format!("return fx_mul({}, P[{b}]) + P[{}];", emit_in(input, "x", "z"), b + 1)
        }
        NodeKind::Clamp { input, .. } => {
            format!("return fx_clamp({}, P[{b}], P[{}]);", emit_in(input, "x", "z"), b + 1)
        }
        NodeKind::Terrace { input, .. } => format!(
            "return fx_mul(fx_round(fx_mul({}, P[{b}])) << 16u, P[{}]);",
            emit_in(input, "x", "z"),
            b + 1
        ),
        NodeKind::Add { a, b: bb } => bin("+", a, bb),
        NodeKind::Sub { a, b: bb } => bin("-", a, bb),
        NodeKind::Mul { a, b: bb } => {
            format!("return fx_mul({}, {});", emit_in(a, "x", "z"), emit_in(bb, "x", "z"))
        }
        NodeKind::Min { a, b: bb } => {
            format!("return min({}, {});", emit_in(a, "x", "z"), emit_in(bb, "x", "z"))
        }
        NodeKind::Max { a, b: bb } => {
            format!("return max({}, {});", emit_in(a, "x", "z"), emit_in(bb, "x", "z"))
        }
        NodeKind::Lerp { a, b: bb, t } => format!(
            "let va = {}; let vb = {}; let tt = fx_clamp({}, 0, FX_ONE); \
             return fx_lerp(va, vb, tt);",
            emit_in(a, "x", "z"),
            emit_in(bb, "x", "z"),
            emit_in(t, "x", "z"),
        ),
        NodeKind::DomainWarp { input, wx, wz, .. } => format!(
            "let nx = x + fx_mul({}, P[{b}]); let nz = z + fx_mul({}, P[{b}]); return {};",
            emit_in(wx, "x", "z"),
            emit_in(wz, "x", "z"),
            emit_in(input, "nx", "nz"),
        ),
        NodeKind::Power { .. } | NodeKind::RadialFalloff { .. } => {
            unreachable!("rejected by validate")
        }
    };
    s.push_str(&body);
    s.push_str(" }\n");
}

fn bin(op: &str, a: &In, b: &In) -> String {
    format!("return {} {op} {};", emit_in(a, "x", "z"), emit_in(b, "x", "z"))
}

/// A wired input becomes a call to its node function at `(xvar, zvar)`; an
/// unwired input is its quantized literal default.
fn emit_in(slot: &In, xvar: &str, zvar: &str) -> String {
    match slot.node {
        Some(j) => format!("node_{j}({xvar}, {zvar})"),
        None => format!("{}", fx::from_f32(slot.default)),
    }
}

/// Topological order (children first) via iterative post-order DFS. The graph
/// is acyclic (checked when lowering), so this terminates.
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

/// Bindings for [`generate_columns`]: integer origin/step so the sample grid
/// is exact; seed as a plain uniform u32.
const COLUMNS_HEADER: &str = r#"
struct View { origin_x: i32, origin_z: i32, step: i32, res: u32, seed: u32, pad0: u32, pad1: u32, pad2: u32 };
@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<storage, read> P: array<i32>;
@group(0) @binding(2) var<storage, read_write> out_height: array<i32>;
@group(0) @binding(3) var<storage, read_write> out_rock: array<i32>;
@group(0) @binding(4) var<storage, read_write> out_structure: array<i32>;
var<private> SEED: u32;
"#;

const COLUMNS_MAIN: &str = r#"
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= view.res || gid.y >= view.res) { return; }
    SEED = view.seed;
    // World voxel coordinate of the column, shifted into Q16.16 (wrapping,
    // matching the CPU's wrapping_shl envelope semantics).
    let x = (view.origin_x + i32(gid.x) * view.step) << 16u;
    let z = (view.origin_z + i32(gid.y) * view.step) << 16u;
    let idx = gid.y * view.res + gid.x;
    out_height[idx] = height_out(x, z);
    out_rock[idx] = rock_out(x, z);
    out_structure[idx] = structure_out(x, z);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_align_with_default_graph() {
        let g = TerrainGraph::default_soils();
        let p = collect_params(&g);
        let total: usize = g.nodes.iter().map(|n| node_params(&n.kind).len()).sum();
        assert_eq!(p.len(), total);
        let src = generate_columns(&g).unwrap();
        assert!(src.contains("fn main"));
        assert!(src.contains("height_out"));
        assert!(src.contains("dn_noise2"));
    }

    #[test]
    fn params_quantize_like_cpu_compile() {
        // The Simplex2 frequency must land as the same integer the CPU
        // evaluator uses (1/1000 -> 66; the documented retune drift).
        let g = TerrainGraph::default_soils();
        let p = collect_params(&g);
        assert_eq!(p[0], 66);
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
