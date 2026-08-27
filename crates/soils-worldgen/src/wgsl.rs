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
        // f32 design-tool noise: parameters cross the boundary as raw f32 bit
        // patterns (`bitcast<f32>` on the GPU side) so both ends consume the
        // identical f32 — Q16.16 quantization would shift the noise character.
        NodeKind::Noise { frequency, offset, param, .. } => {
            vec![fbits(frequency), fbits(offset[0]), fbits(offset[1]), fbits(param)]
        }
        NodeKind::FractalNoise { base_frequency, lacunarity, persistence, offset, param, .. } => {
            vec![
                fbits(base_frequency),
                fbits(lacunarity),
                fbits(persistence),
                fbits(offset[0]),
                fbits(offset[1]),
                fbits(param),
            ]
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

/// An f32 parameter as its raw bit pattern (see the `Noise` arms above).
fn fbits(v: f32) -> i32 {
    v.to_bits() as i32
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
    if uses_hash_noise(graph) {
        s.push_str(HASH_NOISE);
    }
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

/// Generate the full chunk-generation compute shader: `gen_lattice` samples
/// the 9³ cave lattice, `gen_fill` evaluates columns + the soil gradient and
/// writes the packed 32³ voxel volume — the same pure function as
/// [`crate::terrain::TerrainGen::generate`], bit for bit (asserted by the
/// terrainlab `gen_gpu` test). One thread owns each output u32 (4 x-adjacent
/// voxels), so there are no atomics and no write races.
pub fn generate_chunk(graph: &TerrainGraph) -> Result<String, String> {
    // Chunk generation is the bit-exact game path: design-only f32 noise
    // nodes must never reach it (a GPU-born chunk could then differ from the
    // server's CPU chunk by an ULP-flipped voxel).
    graph.deterministic()?;
    let caves = graph.caves;
    let mut s = String::new();
    let _ = writeln!(s, "const CAVES_ON: bool = {};", caves.enabled);
    let _ = writeln!(s, "const CAVE_FREQ: i32 = {};", fx::from_f32(caves.frequency));
    let _ = writeln!(s, "const CAVE_THR: i32 = {};", fx::from_f32(caves.threshold));
    let _ = writeln!(s, "const MAX_SURFACE: i32 = {};", crate::terrain::MAX_SURFACE);
    let _ = writeln!(s, "const MAX_ROCK: i32 = {};", crate::terrain::MAX_ROCK);
    s.push_str(CHUNK_HEADER);
    s.push_str(&emit_functions(graph)?);
    s.push_str(CHUNK_MAIN);
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
        // f32 design-tool noise — mirrors `CKind::NoiseF32`/`FractalF32`:
        // exact Q16.16 → f32 coordinate conversion (power-of-two divide),
        // f32 evaluation, `nm_fx` quantization back.
        NodeKind::Noise { mode, .. } => {
            let f = mode_fn(*mode);
            format!(
                "let xf = f32(x) / 65536.0; let zf = f32(z) / 65536.0; \
                 return nm_fx({f}(vec2<f32>(\
                 xf * bitcast<f32>(P[{b}]) + bitcast<f32>(P[{}]), \
                 zf * bitcast<f32>(P[{b}]) + bitcast<f32>(P[{}])), bitcast<f32>(P[{}])));",
                b + 1,
                b + 2,
                b + 3,
            )
        }
        NodeKind::FractalNoise { mode, octaves, .. } => {
            let f = mode_fn(*mode);
            format!(
                "let xf = f32(x) / 65536.0; let zf = f32(z) / 65536.0; \
                 var ff = bitcast<f32>(P[{b}]); var amp = 1.0; var acc = 0.0; \
                 for (var o = 0u; o < {octaves}u; o = o + 1u) {{ \
                 acc = acc + amp * {f}(vec2<f32>(\
                 xf * ff + bitcast<f32>(P[{}]), zf * ff + bitcast<f32>(P[{}])), \
                 bitcast<f32>(P[{}])); \
                 ff = ff * bitcast<f32>(P[{}]); amp = amp * bitcast<f32>(P[{}]); }} \
                 return nm_fx(acc);",
                b + 3,
                b + 4,
                b + 5,
                b + 1,
                b + 2,
            )
        }
        NodeKind::Power { .. } | NodeKind::RadialFalloff { .. } => {
            unreachable!("rejected by validate")
        }
    };
    s.push_str(&body);
    s.push_str(" }\n");
}

/// The WGSL function name (in [`HASH_NOISE`]) for a noise mode. Each returns
/// signed `~[-1, 1]`, matching `crate::noise_modes::eval_mode`.
fn mode_fn(mode: crate::graph::NoiseMode) -> &'static str {
    use crate::graph::NoiseMode;
    match mode {
        NoiseMode::Value => "nmode_value",
        NoiseMode::Perlin => "nmode_perlin",
        NoiseMode::Simplex => "nmode_simplex",
        NoiseMode::Worley => "nmode_worley",
        NoiseMode::Voronoi => "nmode_voronoi",
        NoiseMode::Gabor => "nmode_gabor",
        NoiseMode::Crater => "nmode_crater",
        NoiseMode::Wool => "nmode_wool",
        NoiseMode::Stone => "nmode_stone",
        NoiseMode::Wavelet => "nmode_wavelet",
    }
}

/// Whether any node needs the f32 hash-noise library — [`HASH_NOISE`] is
/// ~230 lines, only worth parsing when something calls into it.
fn uses_hash_noise(graph: &TerrainGraph) -> bool {
    graph
        .nodes
        .iter()
        .any(|n| matches!(n.kind, NodeKind::Noise { .. } | NodeKind::FractalNoise { .. }))
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

/// Bindings for [`generate_chunk`]. The kernels are job-arrays: one
/// `jobs[i] = (origin.xyz, out slot)` entry per chunk, dispatched
/// `gen_lattice(1, n)` then `gen_fill(1, 4, n)`. `out_voxels` is
/// slot-addressed (8192 words per slot) so the client binds its pooled voxel
/// buffer directly; a standalone harness uses slot 0 into a chunk-sized
/// buffer. `occ[1 + job]` accumulates the chunk's non-air cell count (word 0
/// is reserved for the caller's batch tag; the caller zeroes the buffer);
/// `flags` bit 0 = flat world; palette block ids packed into `pal0`/`pal1`
/// (grass, slate, stone, rocky | tough, dirt).
const CHUNK_HEADER: &str = r#"
struct GenView {
    flags: u32, seed: u32, pal0: u32, pal1: u32,
};
@group(0) @binding(0) var<uniform> gen: GenView;
@group(0) @binding(1) var<storage, read> P: array<i32>;
@group(0) @binding(2) var<storage, read_write> lattice: array<i32>;    // jobs x 9^3
@group(0) @binding(3) var<storage, read_write> out_voxels: array<u32>; // 8192 words per slot
@group(0) @binding(4) var<storage, read> jobs: array<vec4<i32>>;       // origin.xyz, slot
@group(0) @binding(5) var<storage, read_write> occ: array<atomic<u32>>; // [tag, per-job non-air]
var<private> SEED: u32;
"#;

const CHUNK_MAIN: &str = r#"
const CAVE_STEP: i32 = 4;
const CAVE_N: i32 = 9;

fn cave_noise_at(gx: i32, gy: i32, gz: i32) -> i32 {
    return dn_noise3(gen.seed, gx * CAVE_FREQ, gy * CAVE_FREQ, gz * CAVE_FREQ);
}

@compute @workgroup_size(64)
fn gen_lattice(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) t: u32) {
    // 729 samples over one workgroup of 64 threads; one workgroup per job.
    let job = wg.y;
    let o = jobs[job].xyz;
    for (var i = i32(t); i < CAVE_N * CAVE_N * CAVE_N; i = i + 64) {
        let iy = i / (CAVE_N * CAVE_N);
        let rem = i % (CAVE_N * CAVE_N);
        let iz = rem / CAVE_N;
        let ix = rem % CAVE_N;
        lattice[i32(job) * (CAVE_N * CAVE_N * CAVE_N) + i] = cave_noise_at(
            o.x + ix * CAVE_STEP,
            o.y + iy * CAVE_STEP,
            o.z + iz * CAVE_STEP,
        );
    }
}

// Trilinear interpolation over the lattice; fractions are exact Q16.16
// multiples of 1/CAVE_STEP. Mirrors terrain.rs cave_at.
fn cave_at(job: u32, x: i32, y: i32, z: i32) -> i32 {
    let xi = x / CAVE_STEP; let yi = y / CAVE_STEP; let zi = z / CAVE_STEP;
    let fxq = (x % CAVE_STEP) * (FX_ONE / CAVE_STEP);
    let fyq = (y % CAVE_STEP) * (FX_ONE / CAVE_STEP);
    let fzq = (z % CAVE_STEP) * (FX_ONE / CAVE_STEP);
    let x00 = fx_lerp(lat_at(job, xi, yi, zi), lat_at(job, xi + 1, yi, zi), fxq);
    let x10 = fx_lerp(lat_at(job, xi, yi + 1, zi), lat_at(job, xi + 1, yi + 1, zi), fxq);
    let x01 = fx_lerp(lat_at(job, xi, yi, zi + 1), lat_at(job, xi + 1, yi, zi + 1), fxq);
    let x11 = fx_lerp(lat_at(job, xi, yi + 1, zi + 1), lat_at(job, xi + 1, yi + 1, zi + 1), fxq);
    return fx_lerp(fx_lerp(x00, x10, fyq), fx_lerp(x01, x11, fyq), fzq);
}

fn lat_at(job: u32, ix: i32, iy: i32, iz: i32) -> i32 {
    return lattice[i32(job) * (CAVE_N * CAVE_N * CAVE_N) + (iy * CAVE_N + iz) * CAVE_N + ix];
}

// Soil gradient by depth below the surface; mirrors terrain.rs generate_with.
fn soil(gy: i32, height: i32) -> u32 {
    if (gy > height) { return 0u; }
    if (gy == height) { return gen.pal0 & 0xffu; }          // grass
    if (gy < height - 64) { return (gen.pal0 >> 8u) & 0xffu; }  // slate
    if (gy < height - 32) { return (gen.pal0 >> 16u) & 0xffu; } // stone
    if (gy < height - 16) { return (gen.pal0 >> 24u) & 0xffu; } // rocky dirt
    if (gy < height - 8) { return gen.pal1 & 0xffu; }           // tough dirt
    return (gen.pal1 >> 8u) & 0xffu;                            // dirt
}

@compute @workgroup_size(8, 8)
fn gen_fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    // One thread per output word: 4 x-adjacent voxels at (z, y..); one job
    // per z-layer of the dispatch (1, 4, jobs).
    SEED = gen.seed;
    let job = gid.z;
    let o = jobs[job].xyz;
    let slot = u32(jobs[job].w);
    let xw = i32(gid.x);   // word column 0..8
    let z = i32(gid.y);    // 0..32
    if (xw >= 8 || z >= 32) { return; }
    let flat = (gen.flags & 1u) == 1u;
    let stone = (gen.pal0 >> 16u) & 0xffu;

    let ceiling = select(MAX_SURFACE, 256, flat);
    let all_air = o.y > ceiling;

    var heights: array<i32, 4>;
    var rocks: array<i32, 4>;
    for (var k = 0; k < 4; k = k + 1) {
        let gx = o.x + xw * 4 + k;
        let gz = o.z + z;
        if (flat) {
            heights[k] = 256; rocks[k] = 0;
        } else {
            heights[k] = fx_floor(height_out(gx << 16u, gz << 16u));
            rocks[k] = rock_out(gx << 16u, gz << 16u);
        }
    }

    var non_air = 0u;
    for (var y = 0; y < 32; y = y + 1) {
        let gy = o.y + y;
        var word = 0u;
        if (!all_air) {
            for (var k = 0; k < 4; k = k + 1) {
                let x = xw * 4 + k;
                let height = heights[k];
                var val = soil(gy, height);
                if (!flat && CAVES_ON) {
                    // Surface rock outcrops.
                    if (gy > height - 2 && ((gy - height) << 16u) <= rocks[k]) {
                        val = stone;
                    }
                    // Caves carved from solid ground.
                    if (val != 0u && fx_abs(cave_at(job, x, y, z)) > CAVE_THR) {
                        val = 0u;
                    }
                }
                if (val != 0u) { non_air = non_air + 1u; }
                word = word | (val << (u32(k) * 8u));
            }
        }
        out_voxels[slot * 8192u + u32((y + z * 32) * 8 + xw)] = word;
    }
    if (non_air > 0u) {
        atomicAdd(&occ[1u + job], non_air);
    }
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
        // Every Simplex2 frequency must land as the same integer the CPU
        // evaluator uses, or the GPU mirror generates different terrain.
        // Params are emitted in node order and a Simplex2 contributes three
        // (frequency then a 2D offset), so p[0] is the continental octave's
        // frequency (1/2000 -> 33); 1/1000 -> 66 is the documented retune
        // drift and must still appear.
        let g = TerrainGraph::default_soils();
        let p = collect_params(&g);
        assert_eq!(p[0], 33, "continental octave frequency");
        assert!(p.contains(&66), "1/1000 octave frequency missing from {p:?}");
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

/// The ported f32 hash-noise library (design-tool `Noise` / `FractalNoise`
/// nodes) — the WGSL twin of [`crate::noise_modes`], appended to the prelude
/// only when a graph uses those nodes. Unlike everything above, this is plain
/// f32 math: ULP-close to the CPU (the integer-lattice hash itself is exact;
/// see `noise_modes` for why float-bit hashing was replaced), but not bit-
/// exact — such graphs are rejected by [`TerrainGraph::deterministic`] for
/// the game path. `nm_fx` mirrors `fx::from_f32` at the boundary.
const HASH_NOISE: &str = r#"
const NM_INV_U32: f32 = 1.0 / 4294967296.0;
fn nm_ihash(ix: i32, iy: i32) -> u32 {
    var h = bitcast<u32>(ix) * 3242174889u + bitcast<u32>(iy) * 2447445413u;
    h = h ^ (h >> 16u);
    h = h * 2246822507u;
    h = h ^ (h >> 13u);
    h = h * 3266489909u;
    h = h ^ (h >> 16u);
    return h;
}
fn nm_lcg(h: u32) -> u32 { return h * 1664525u + 1013904223u; }
fn nm_hash12(p: vec2<f32>) -> f32 {
    return f32(nm_ihash(i32(p.x), i32(p.y))) * NM_INV_U32;
}
fn nm_hash22(p: vec2<f32>) -> vec2<f32> {
    let h = nm_ihash(i32(p.x), i32(p.y));
    return vec2<f32>(f32(h), f32(nm_lcg(h))) * NM_INV_U32;
}
fn nm_hash32(p: vec2<f32>) -> vec3<f32> {
    let h = nm_ihash(i32(p.x), i32(p.y));
    let h2 = nm_lcg(h);
    let h3 = nm_lcg(h2);
    return vec3<f32>(f32(h), f32(h2), f32(h3)) * NM_INV_U32;
}
fn nm_sstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}
fn nm_value12(p: vec2<f32>) -> f32 {
    let i = floor(p);
    var f = p - i;
    f = f * f * (vec2<f32>(3.0) - 2.0 * f);
    return mix(mix(nm_hash12(i), nm_hash12(i + vec2<f32>(1.0, 0.0)), f.x),
               mix(nm_hash12(i + vec2<f32>(0.0, 1.0)), nm_hash12(i + vec2<f32>(1.0, 1.0)), f.x), f.y);
}
fn nm_perlin12(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = p - i;
    let u = f * f * f * (vec2<f32>(10.0) + f * (6.0 * f - 15.0));
    let ga = normalize(nm_hash22(i + vec2<f32>(0.0, 0.0)) - 0.5);
    let gb = normalize(nm_hash22(i + vec2<f32>(1.0, 0.0)) - 0.5);
    let gc = normalize(nm_hash22(i + vec2<f32>(0.0, 1.0)) - 0.5);
    let gd = normalize(nm_hash22(i + vec2<f32>(1.0, 1.0)) - 0.5);
    let a = dot(ga, f - vec2<f32>(0.0, 0.0));
    let b = dot(gb, f - vec2<f32>(1.0, 0.0));
    let c = dot(gc, f - vec2<f32>(0.0, 1.0));
    let d = dot(gd, f - vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y) * 0.7 + 0.5;
}
fn nm_perlin12d(p: vec2<f32>) -> vec3<f32> {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let du = 30.0 * f * f * (f * (f - 2.0) + 1.0);
    let ga = nm_hash22(i + vec2<f32>(0.0, 0.0)) * 2.0 - 1.0;
    let gb = nm_hash22(i + vec2<f32>(1.0, 0.0)) * 2.0 - 1.0;
    let gc = nm_hash22(i + vec2<f32>(0.0, 1.0)) * 2.0 - 1.0;
    let gd = nm_hash22(i + vec2<f32>(1.0, 1.0)) * 2.0 - 1.0;
    let va = dot(ga, f - vec2<f32>(0.0, 0.0));
    let vb = dot(gb, f - vec2<f32>(1.0, 0.0));
    let vc = dot(gc, f - vec2<f32>(0.0, 1.0));
    let vd = dot(gd, f - vec2<f32>(1.0, 1.0));
    let value = va + u.x * (vb - va) + u.y * (vc - va) + u.x * u.y * (va - vb - vc + vd);
    let k = va - vb - vc + vd;
    let deriv = ga + u.x * (gb - ga) + u.y * (gc - ga) + u.x * u.y * (ga - gb - gc + gd)
        + du * (u.yx * k + vec2<f32>(vb, vc) - va);
    return vec3<f32>(value, deriv.x, deriv.y);
}
fn nm_simplex12(p: vec2<f32>) -> f32 {
    let i = floor(p + (p.x + p.y) * 0.366025);
    let a = p - i + (i.x + i.y) * 0.211324;
    let m = step(a.y, a.x);
    let o = vec2<f32>(m, 1.0 - m);
    let b = a - o + 0.211324;
    let c = a - 0.577351;
    let h = max(vec3<f32>(0.5) - vec3<f32>(dot(a, a), dot(b, b), dot(c, c)), vec3<f32>(0.0));
    let n = h * h * h * h * vec3<f32>(dot(a, nm_hash22(i) - 0.5),
                                      dot(b, nm_hash22(i + o) - 0.5),
                                      dot(c, nm_hash22(i + 1.0) - 0.5));
    return dot(n, vec3<f32>(70.0)) + 0.5;
}
fn nm_worley12(pp: vec2<f32>) -> f32 {
    let i = floor(pp);
    let p = pp - i;
    var w = 1e6;
    for (var x = -1; x <= 1; x = x + 1) {
        for (var y = -1; y <= 1; y = y + 1) {
            let g = vec2<f32>(f32(x), f32(y));
            let hh = nm_hash12(i + g);
            let c = p - g - vec2<f32>(hh);
            w = min(w, dot(c, c));
        }
    }
    return 1.0 - sqrt(w);
}
fn nm_voronoi12(x: vec2<f32>, sm: f32) -> f32 {
    let s = 1.0 / max(sm, 1e-3);
    let p = floor(x);
    let f = x - p;
    var va = 0.0;
    var wt = 0.0;
    for (var xi = -1; xi <= 1; xi = xi + 1) {
        for (var yi = -1; yi <= 1; yi = yi + 1) {
            let g = vec2<f32>(f32(xi), f32(yi));
            let o = nm_hash32(p + g);
            let d = length(g - f + o.xy);
            let ww = pow(nm_sstep(1.414, 0.0, d), s);
            va = va + o.z * ww;
            wt = wt + ww;
        }
    }
    return va / max(wt, 1e-6);
}
fn nm_sinr(a: f32) -> f32 {
    return sin(a - round(a * 0.15915494) * 6.2831855);
}
fn nm_gabor12(p: vec2<f32>) -> f32 {
    let kF = 8.0;
    let i = floor(p);
    var f = p - i;
    f = f * f * (vec2<f32>(3.0) - 2.0 * f);
    let s00 = nm_sinr(kF * dot(p, nm_hash22(i + vec2<f32>(0.0, 0.0))));
    let s10 = nm_sinr(kF * dot(p, nm_hash22(i + vec2<f32>(1.0, 0.0))));
    let s01 = nm_sinr(kF * dot(p, nm_hash22(i + vec2<f32>(0.0, 1.0))));
    let s11 = nm_sinr(kF * dot(p, nm_hash22(i + vec2<f32>(1.0, 1.0))));
    return mix(mix(s00, s10, f.x), mix(s01, s11, f.x), f.y);
}
fn nm_crater12(pin: vec2<f32>) -> f32 {
    let f = fract(pin);
    let p = floor(pin);
    var va = 0.0;
    var wt = 0.0;
    for (var i = -2; i <= 2; i = i + 1) {
        for (var j = -2; j <= 2; j = j + 1) {
            let g = vec2<f32>(f32(i), f32(j));
            let o = nm_hash22(p + g);
            let d = length(f - g - o);
            let w = exp(-4.0 * d);
            va = va + w * sin(6.28 * sqrt(max(d, 0.06)));
            wt = wt + w;
        }
    }
    return abs(va / wt);
}
fn nm_fbm_wool(pin: vec2<f32>) -> vec2<f32> {
    var p = pin;
    var s = vec2<f32>(0.0);
    var m = 0.0;
    var a = 1.0;
    for (var i = 0; i < 6; i = i + 1) {
        let nd = nm_perlin12d(p);
        s = s + a * nd.yz;
        m = m + a;
        a = a * 0.5;
        p = p * 2.0;
    }
    return s / m;
}
fn nm_wool12(p: vec2<f32>) -> f32 {
    let n = nm_fbm_wool(p);
    return max(abs(n.x), abs(n.y));
}
fn nm_fbm_stone(pin: vec2<f32>) -> vec3<f32> {
    var p = pin;
    var s = vec3<f32>(0.0);
    var a = 1.0;
    for (var i = 0; i < 6; i = i + 1) {
        s = s + a * nm_perlin12d(p);
        a = a * 0.5;
        p = p * 2.0;
    }
    return s;
}
fn nm_fbm12(pin: vec2<f32>, octaves: i32) -> f32 {
    var p = pin;
    var s = 0.0;
    var m = 0.0;
    var a = 1.0;
    for (var i = 0; i < octaves; i = i + 1) {
        s = s + a * nm_perlin12(p);
        m = m + a;
        a = a * 0.5;
        p = p * 2.0;
    }
    return s / m;
}
fn nm_stone12(p: vec2<f32>) -> f32 {
    let d = nm_fbm_stone(p);
    return nm_fbm12(p + d.yz * 0.4, 6);
}
fn nm_wavelet12(pin: vec2<f32>, phase: f32) -> f32 {
    let scale = 1.24;
    var p = pin;
    var d = 0.0;
    var s = 1.0;
    var m = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        let fi = f32(i);
        let q0 = p * s;
        var g = fract(floor(q0) * vec2<f32>(123.34, 233.53));
        let gd = dot(g, g + vec2<f32>(23.234));
        g = g + vec2<f32>(gd);
        let a = fract(g.x * g.y) * 1000.0;
        let ca = cos(a);
        let sa = sin(a);
        let r = fract(q0) - vec2<f32>(0.5);
        let q = vec2<f32>(r.x * ca - r.y * sa, r.x * sa + r.y * ca);
        d = d + sin(q.x * 10.0 + phase) * nm_sstep(0.25, 0.0, dot(q, q)) / s;
        p = vec2<f32>(0.54 * p.x - 0.84 * p.y + fi, 0.84 * p.x + 0.54 * p.y + fi);
        m = m + 1.0 / s;
        s = s * scale;
    }
    return d / m;
}
fn nmode_value(p: vec2<f32>, prm: f32) -> f32 { return nm_value12(p) * 2.0 - 1.0; }
fn nmode_perlin(p: vec2<f32>, prm: f32) -> f32 { return nm_perlin12(p) * 2.0 - 1.0; }
fn nmode_simplex(p: vec2<f32>, prm: f32) -> f32 { return nm_simplex12(p) * 2.0 - 1.0; }
fn nmode_worley(p: vec2<f32>, prm: f32) -> f32 { return nm_worley12(p) * 2.0 - 1.0; }
fn nmode_voronoi(p: vec2<f32>, prm: f32) -> f32 { return nm_voronoi12(p, prm) * 2.0 - 1.0; }
fn nmode_gabor(p: vec2<f32>, prm: f32) -> f32 { return nm_gabor12(p); }
fn nmode_crater(p: vec2<f32>, prm: f32) -> f32 { return nm_crater12(p) * 2.0 - 1.0; }
fn nmode_wool(p: vec2<f32>, prm: f32) -> f32 { return nm_wool12(p) * 2.0 - 1.0; }
fn nmode_stone(p: vec2<f32>, prm: f32) -> f32 { return nm_stone12(p) * 2.0 - 1.0; }
fn nmode_wavelet(p: vec2<f32>, prm: f32) -> f32 { return nm_wavelet12(p, prm); }
fn nm_fx(v: f32) -> i32 { return i32(round(v * 65536.0)); }
"#;
