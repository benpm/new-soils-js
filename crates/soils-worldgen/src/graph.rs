//! A serializable node graph describing terrain generation, plus the compiled
//! fixed-point evaluator ([`CompiledGraph`]) that is **bit-exact** with the
//! WGSL codegen in [`crate::wgsl`] — CPU (client + server) and GPU generate
//! identical chunks from a seed (worldgen v2).
//!
//! # Model
//!
//! The graph is a DAG of nodes. The unifying abstraction is that **every node
//! is a pure scalar function of a 2D world coordinate**: `f(x, z) -> f32`.
//! Sources like [`NodeKind::Coord`] read the coordinate directly; combinators
//! read their inputs (other nodes) at the same coordinate; [`NodeKind::DomainWarp`]
//! is the one node that samples its input at a *shifted* coordinate. Because
//! evaluation is coordinate-parameterized rather than a bottom-up fold, domain
//! warping composes naturally and the same shape translates 1:1 to a set of
//! WGSL `fn node_N(x, z)` functions on the GPU.
//!
//! Named [`Outputs`] pick which nodes feed the terrain channels (height, rock
//! outcrop amount, structure/scatter density). Caves are a separate fixed 3D
//! simplex carve ([`CaveParams`]) because the node graph itself is 2D.
//!
//! [`TerrainGraph::default_soils`] reconstructs the original hardcoded
//! `terrain.rs` formulas node-for-node (character-equivalent under the v2
//! noise core).

use serde::{Deserialize, Serialize};

use crate::fx::{self, Fx};
use crate::noise_det;

/// Index of a node within [`TerrainGraph::nodes`]. The canonical form keeps
/// `nodes[i].id == i`; [`TerrainGraph::validate`] enforces this.
pub type NodeId = usize;

/// Which world axis a [`NodeKind::Coord`] source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    X,
    Z,
}

/// A base noise function for [`NodeKind::Noise`] / [`NodeKind::FractalNoise`],
/// ported from `noise.glsl` to both the CPU ([`crate::noise_modes`]) and the
/// GPU (`crate::wgsl`'s `HASH_NOISE`) so the two agree. All output signed
/// `~[-1, 1]`. Design-tool only for now: f32-evaluated, so not bit-exact
/// across devices — see [`TerrainGraph::deterministic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoiseMode {
    /// Smooth value noise (Hermite-interpolated cell hashes).
    Value,
    /// Classic Perlin gradient noise.
    Perlin,
    /// Hash-based simplex noise.
    Simplex,
    /// Cellular / Worley F1 (nearest feature-point distance).
    Worley,
    /// Smooth Voronoi; `param` is edge smoothness (default 0.5).
    Voronoi,
    /// Sinusoidal Gabor-like bands.
    Gabor,
    /// Impact-crater field (rings).
    Crater,
    /// Fibrous derivative noise.
    Wool,
    /// Domain-warped fBm (rock/stone look).
    Stone,
    /// Rotated wavelet noise; `param` is phase (default 0). NB: discontinuous
    /// (per-cell random rotation), so its GPU 3D preview can differ from the CPU
    /// map by ~0.02 at cell boundaries — the only mode not held to GPU/CPU parity.
    Wavelet,
}

impl NoiseMode {
    /// All modes, in palette/dropdown order.
    pub const ALL: [NoiseMode; 10] = [
        NoiseMode::Value,
        NoiseMode::Perlin,
        NoiseMode::Simplex,
        NoiseMode::Worley,
        NoiseMode::Voronoi,
        NoiseMode::Gabor,
        NoiseMode::Crater,
        NoiseMode::Wool,
        NoiseMode::Stone,
        NoiseMode::Wavelet,
    ];

    /// Human-readable name (used for node titles and the editor dropdown).
    pub fn label(self) -> &'static str {
        match self {
            NoiseMode::Value => "Value",
            NoiseMode::Perlin => "Perlin",
            NoiseMode::Simplex => "Simplex",
            NoiseMode::Worley => "Worley",
            NoiseMode::Voronoi => "Voronoi",
            NoiseMode::Gabor => "Gabor",
            NoiseMode::Crater => "Crater",
            NoiseMode::Wool => "Wool",
            NoiseMode::Stone => "Stone",
            NoiseMode::Wavelet => "Wavelet",
        }
    }

    /// Label for the mode-specific `param` slider, or `None` if the mode ignores
    /// `param`.
    pub fn param_label(self) -> Option<&'static str> {
        match self {
            NoiseMode::Voronoi => Some("smoothness"),
            NoiseMode::Wavelet => Some("phase"),
            _ => None,
        }
    }
}

/// An input slot on a node: either wired to another node's output, or left
/// unwired (in which case `default` is used). Keeping a literal fallback on
/// every slot means a partially-wired graph still evaluates, which matches how
/// the node editor behaves while you build a graph up.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct In {
    /// Source node, or `None` to use `default`.
    pub node: Option<NodeId>,
    /// Value used when `node` is `None`.
    pub default: f32,
}

impl In {
    /// An unwired slot with a constant fallback.
    pub const fn constant(v: f32) -> Self {
        Self { node: None, default: v }
    }
    /// A slot wired to `id`.
    pub const fn from(id: NodeId) -> Self {
        Self { node: Some(id), default: 0.0 }
    }
}

/// The operation a node performs. Every variant is a pure function of the
/// evaluation coordinate `(x, z)` and its wired inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeKind {
    // ---- sources (ignore or read the coordinate) ----
    /// A literal value, independent of position.
    Constant { value: f32 },
    /// The world coordinate along `axis`.
    Coord { axis: Axis },
    /// 2D gradient noise sampled at `(x, z) * frequency + offset`. `offset`
    /// gives a cheap way to decorrelate octaves/features without a second
    /// seed. (Named for the original simplex implementation; v2 evaluates
    /// `noise_det::noise2`, the deterministic fixed-point core.)
    Simplex2 { frequency: f32, offset: [f32; 2] },
    /// Fractal Brownian motion: `octaves` of simplex with `lacunarity` /
    /// `persistence`, the node the original `terrain.rs` hand-unrolled.
    Fbm { octaves: u32, base_frequency: f32, lacunarity: f32, persistence: f32, offset: [f32; 2] },
    /// One of the [`NoiseMode`] functions sampled at `(x, z) * frequency +
    /// offset`. `param` is the mode's extra scalar (Voronoi smoothness / Wavelet
    /// phase), ignored by other modes.
    Noise { mode: NoiseMode, frequency: f32, offset: [f32; 2], param: f32 },
    /// Fractal stack of a [`NoiseMode`] — like [`NodeKind::Fbm`] but over any
    /// ported mode.
    FractalNoise {
        mode: NoiseMode,
        octaves: u32,
        base_frequency: f32,
        lacunarity: f32,
        persistence: f32,
        offset: [f32; 2],
        param: f32,
    },
    /// Radial island falloff: `1` near `center`, decaying to `0` past `radius`
    /// with the given `exponent`. Multiply into height for islands.
    RadialFalloff { center: [f32; 2], radius: f32, exponent: f32 },

    // ---- unary modulators ----
    Abs { input: In },
    /// `input * scale + bias`.
    ScaleBias { input: In, scale: f32, bias: f32 },
    Clamp { input: In, min: f32, max: f32 },
    Power { input: In, exponent: f32 },
    /// Quantize into `steps` flat terraces over `[-1, 1]`-ish range.
    Terrace { input: In, steps: f32 },

    // ---- combinators ----
    Add { a: In, b: In },
    Sub { a: In, b: In },
    Mul { a: In, b: In },
    Min { a: In, b: In },
    Max { a: In, b: In },
    /// `a + (b - a) * clamp(t, 0, 1)`.
    Lerp { a: In, b: In, t: In },

    /// Sample `input` at a coordinate offset by `(wx, wz) * amount`.
    DomainWarp { input: In, wx: In, wz: In, amount: f32 },
}

/// One node in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Must equal the node's index in [`TerrainGraph::nodes`].
    pub id: NodeId,
    pub kind: NodeKind,
}

/// Which nodes drive each terrain channel. `height` is required; the rest are
/// optional (a graph with no `structure` output simply scatters nothing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outputs {
    /// Surface height in world Y (before flooring).
    pub height: In,
    /// Surface rock-outcrop amount, added to height when testing for stone.
    pub rock: Option<In>,
    /// Structure/scatter density field (e.g. tree density), in `[0, 1]`.
    pub structure: Option<In>,
}

/// Fixed 3D cave carve. Not part of the 2D node graph, but tunable and
/// serialized alongside it so a saved graph fully describes a world.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CaveParams {
    pub enabled: bool,
    /// Coordinate scale (`gx * frequency`) of the 3D simplex.
    pub frequency: f32,
    /// `|noise|` above this carves air.
    pub threshold: f32,
}

impl Default for CaveParams {
    fn default() -> Self {
        // Wavelength ~45 voxels like the original. The threshold is tuned to
        // the v2 noise's amplitude distribution: measured densities over deep
        // chunks (see terrain::tests::measure_cave_density) — 0.50 → 2.2%,
        // 0.52 → ~1.5%, 0.55 → 0.9% — targeting the original's ~1-2%.
        Self { enabled: true, frequency: 1.0 / 45.0, threshold: 0.52 }
    }
}

/// A complete, serializable terrain description shared by the design tool and
/// the game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerrainGraph {
    pub nodes: Vec<Node>,
    pub outputs: Outputs,
    #[serde(default)]
    pub caves: CaveParams,
}

/// Per-column sample produced by the 2D graph.
#[derive(Debug, Clone, Copy)]
pub struct ColumnSample {
    /// Surface height in world Y (not yet floored).
    pub height: f64,
    /// Rock-outcrop amount at this column.
    pub rock: f64,
    /// Structure/scatter density in `[0, 1]` (0 if the graph has no output).
    pub structure: f64,
}

impl TerrainGraph {
    /// Check the canonical invariant (`nodes[i].id == i`), that every wired
    /// input references an existing node, and that every node kind is
    /// supported by the deterministic v2 evaluator. Returns the first problem.
    pub fn validate(&self) -> Result<(), String> {
        for (i, n) in self.nodes.iter().enumerate() {
            if n.id != i {
                return Err(format!("node at index {i} has id {} (must be {i})", n.id));
            }
        }
        let n = self.nodes.len();
        let check = |slot: &In| -> Result<(), String> {
            match slot.node {
                Some(id) if id >= n => Err(format!("input references missing node {id}")),
                _ => Ok(()),
            }
        };
        for node in &self.nodes {
            // Power / RadialFalloff need pow/sqrt, which have no fixed-point
            // WGSL mirror yet (worldgen v2 must be bit-exact CPU==GPU).
            if matches!(node.kind, NodeKind::Power { .. } | NodeKind::RadialFalloff { .. }) {
                return Err(format!(
                    "node {} kind is not supported by deterministic worldgen v2 yet",
                    node.id
                ));
            }
            for slot in node.kind.inputs() {
                check(slot)?;
            }
        }
        check(&self.outputs.height)?;
        if let Some(s) = &self.outputs.rock {
            check(s)?;
        }
        if let Some(s) = &self.outputs.structure {
            check(s)?;
        }
        Ok(())
    }

    /// The stricter gate for the *game* path (chunk generation, `graph_hash`
    /// negotiation): everything [`Self::validate`] checks, plus rejection of
    /// node kinds whose evaluation is not **bit-exact** across CPU and GPU.
    /// The ported f32 hash-noise nodes ([`NodeKind::Noise`] /
    /// [`NodeKind::FractalNoise`]) compile and preview fine in the design tool
    /// — CPU and GPU agree to within f32 rounding — but worldgen v2 requires
    /// byte-identical chunks from a seed on every device, which f32 cannot
    /// promise (drivers may contract/reassociate float math). They stay
    /// design-only until they get a fixed-point port.
    pub fn deterministic(&self) -> Result<(), String> {
        self.validate()?;
        for node in &self.nodes {
            if matches!(node.kind, NodeKind::Noise { .. } | NodeKind::FractalNoise { .. }) {
                return Err(format!(
                    "node {} ({}) is a design-tool noise node: f32-evaluated, not bit-exact \
                     across devices, so it cannot drive deterministic worldgen v2 yet",
                    node.id,
                    match &node.kind {
                        NodeKind::Noise { mode, .. } => format!("Noise/{}", mode.label()),
                        NodeKind::FractalNoise { mode, .. } => format!("Fractal/{}", mode.label()),
                        _ => unreachable!(),
                    }
                ));
            }
        }
        Ok(())
    }

    /// Quantize parameters to Q16.16 and precompute inverses: the form both
    /// the CPU evaluator and the WGSL codegen consume. Call once, evaluate
    /// many. Errors on graphs that fail [`Self::validate`] (e.g. node kinds
    /// the deterministic evaluator doesn't support yet).
    pub fn compile(&self) -> Result<CompiledGraph, String> {
        self.validate()?;
        let cin = |s: &In| match s.node {
            Some(id) => CIn::Node(id),
            None => CIn::Const(fx::from_f32(s.default)),
        };
        let nodes = self
            .nodes
            .iter()
            .map(|n| match &n.kind {
                NodeKind::Constant { value } => CKind::Constant(fx::from_f32(*value)),
                NodeKind::Coord { axis } => CKind::Coord(*axis),
                NodeKind::Simplex2 { frequency, offset } => CKind::Noise2 {
                    freq: fx::from_f32(*frequency),
                    off: [fx::from_f32(offset[0]), fx::from_f32(offset[1])],
                },
                NodeKind::Fbm { octaves, base_frequency, lacunarity, persistence, offset } => {
                    CKind::Fbm {
                        octaves: *octaves,
                        base: fx::from_f32(*base_frequency),
                        lac: fx::from_f32(*lacunarity),
                        per: fx::from_f32(*persistence),
                        off: [fx::from_f32(offset[0]), fx::from_f32(offset[1])],
                    }
                }
                // Ported hash noise evaluates in f32 (see `noise_modes`):
                // deterministic on one platform and ULP-close CPU vs GPU, but
                // NOT bit-exact — [`Self::deterministic`] rejects these kinds
                // for the game path; they are design-tool nodes for now.
                NodeKind::Noise { mode, frequency, offset, param } => CKind::NoiseF32 {
                    mode: *mode,
                    freq: *frequency,
                    off: *offset,
                    param: *param,
                },
                NodeKind::FractalNoise {
                    mode,
                    octaves,
                    base_frequency,
                    lacunarity,
                    persistence,
                    offset,
                    param,
                } => CKind::FractalF32 {
                    mode: *mode,
                    octaves: *octaves,
                    base: *base_frequency,
                    lac: *lacunarity,
                    per: *persistence,
                    off: *offset,
                    param: *param,
                },
                NodeKind::Abs { input } => CKind::Abs(cin(input)),
                NodeKind::ScaleBias { input, scale, bias } => {
                    CKind::ScaleBias(cin(input), fx::from_f32(*scale), fx::from_f32(*bias))
                }
                NodeKind::Clamp { input, min, max } => {
                    CKind::Clamp(cin(input), fx::from_f32(*min), fx::from_f32(*max))
                }
                NodeKind::Terrace { input, steps } => {
                    let s = steps.max(1.0);
                    // 1/s in Q16.16, computed once in f64 (exact quantization).
                    CKind::Terrace(cin(input), fx::from_f32(s), (65536.0 / s as f64).round() as Fx)
                }
                NodeKind::Add { a, b } => CKind::Add(cin(a), cin(b)),
                NodeKind::Sub { a, b } => CKind::Sub(cin(a), cin(b)),
                NodeKind::Mul { a, b } => CKind::Mul(cin(a), cin(b)),
                NodeKind::Min { a, b } => CKind::Min(cin(a), cin(b)),
                NodeKind::Max { a, b } => CKind::Max(cin(a), cin(b)),
                NodeKind::Lerp { a, b, t } => CKind::Lerp(cin(a), cin(b), cin(t)),
                NodeKind::DomainWarp { input, wx, wz, amount } => {
                    CKind::DomainWarp(cin(input), cin(wx), cin(wz), fx::from_f32(*amount))
                }
                NodeKind::Power { .. } | NodeKind::RadialFalloff { .. } => {
                    unreachable!("rejected by validate")
                }
            })
            .collect();
        Ok(CompiledGraph {
            nodes,
            height: cin(&self.outputs.height),
            rock: self.outputs.rock.as_ref().map(cin),
            structure: self.outputs.structure.as_ref().map(cin),
            caves: self.caves.enabled.then(|| CompiledCaves {
                freq: fx::from_f32(self.caves.frequency),
                threshold: fx::from_f32(self.caves.threshold),
            }),
        })
    }

    /// Preview convenience: compile-and-evaluate one node's field at an f64
    /// coordinate (0 for uncompilable graphs). Hot loops should
    /// [`Self::compile`] once instead.
    pub fn field_at(&self, seed: u32, node: NodeId, x: f64, z: f64) -> f64 {
        match self.compile() {
            Ok(c) => fx::to_f32(c.node_fx(seed, node, coord_fx(x), coord_fx(z))) as f64,
            Err(_) => 0.0,
        }
    }

    /// Preview convenience: compile-and-evaluate the surface channels (zeros
    /// for uncompilable graphs).
    pub fn eval_columns(&self, seed: u32, x: f64, z: f64) -> ColumnSample {
        match self.compile() {
            Ok(c) => c.eval_columns(seed, coord_fx(x), coord_fx(z)),
            Err(_) => ColumnSample { height: 0.0, rock: 0.0, structure: 0.0 },
        }
    }

    /// The default graph, reconstructing the original `terrain.rs` height and
    /// rock formulas node-for-node (character-equivalent under the v2 noise;
    /// same octave frequencies and amplitudes).
    ///
    /// Original height:
    /// `256 + floor( s(1/1000)*50 - s(1/500)*30 + s(1/250)*20 - s(1/75)*10 + s(1/25)*5 )`
    /// Original rock: `s(1/15)*5 - |s(1/45)|*10 - |s(1/25)|*15`,
    /// where `s(f) = simplex([gx*f, gz*f])`.
    pub fn default_soils() -> Self {
        let mut nodes: Vec<Node> = Vec::new();
        let mut push = |kind: NodeKind| -> NodeId {
            let id = nodes.len();
            nodes.push(Node { id, kind });
            id
        };
        let simplex = |push: &mut dyn FnMut(NodeKind) -> NodeId, freq: f32| {
            push(NodeKind::Simplex2 { frequency: freq, offset: [0.0, 0.0] })
        };
        let scaled = |push: &mut dyn FnMut(NodeKind) -> NodeId, input: NodeId, scale: f32| {
            push(NodeKind::ScaleBias { input: In::from(input), scale, bias: 0.0 })
        };

        // --- height octaves ---
        let o1 = simplex(&mut push, 1.0 / 1000.0);
        let o1s = scaled(&mut push, o1, 50.0);
        let o2 = simplex(&mut push, 1.0 / 500.0);
        let o2s = scaled(&mut push, o2, -30.0);
        let o3 = simplex(&mut push, 1.0 / 250.0);
        let o3s = scaled(&mut push, o3, 20.0);
        let o4 = simplex(&mut push, 1.0 / 75.0);
        let o4s = scaled(&mut push, o4, -10.0);
        let o5 = simplex(&mut push, 1.0 / 25.0);
        let o5s = scaled(&mut push, o5, 5.0);
        // Sum left-to-right to match the original expression's float association.
        let s1 = push(NodeKind::Add { a: In::from(o1s), b: In::from(o2s) });
        let s2 = push(NodeKind::Add { a: In::from(s1), b: In::from(o3s) });
        let s3 = push(NodeKind::Add { a: In::from(s2), b: In::from(o4s) });
        let s4 = push(NodeKind::Add { a: In::from(s3), b: In::from(o5s) });
        let height = push(NodeKind::ScaleBias { input: In::from(s4), scale: 1.0, bias: 256.0 });

        // --- rock outcrops: s(1/15)*5 - |s(1/45)|*10 - |s(1/25)|*15 ---
        let r1 = simplex(&mut push, 1.0 / 15.0);
        let r1s = scaled(&mut push, r1, 5.0);
        let r2 = simplex(&mut push, 1.0 / 45.0);
        let r2a = push(NodeKind::Abs { input: In::from(r2) });
        let r2s = scaled(&mut push, r2a, -10.0);
        let r3 = simplex(&mut push, 1.0 / 25.0);
        let r3a = push(NodeKind::Abs { input: In::from(r3) });
        let r3s = scaled(&mut push, r3a, -15.0);
        let rk1 = push(NodeKind::Add { a: In::from(r1s), b: In::from(r2s) });
        let rock = push(NodeKind::Add { a: In::from(rk1), b: In::from(r3s) });

        Self {
            nodes,
            outputs: Outputs {
                height: In::from(height),
                rock: Some(In::from(rock)),
                structure: None,
            },
            caves: CaveParams::default(),
        }
    }
}

/// Clamp an f64 preview coordinate into the Q16.16 envelope.
fn coord_fx(v: f64) -> Fx {
    (v * 65536.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as Fx
}

/// A compiled input slot: wired node or quantized constant.
#[derive(Debug, Clone, Copy)]
pub enum CIn {
    Node(NodeId),
    Const(Fx),
}

/// A node with parameters quantized to Q16.16 and inverses precomputed.
#[derive(Debug, Clone)]
pub enum CKind {
    Constant(Fx),
    Coord(Axis),
    Noise2 { freq: Fx, off: [Fx; 2] },
    Fbm { octaves: u32, base: Fx, lac: Fx, per: Fx, off: [Fx; 2] },
    /// Ported hash noise, evaluated in **f32** (params stay f32 and cross the
    /// GPU boundary as raw bit patterns): design-tool only, ULP-close but not
    /// bit-exact across devices — see [`TerrainGraph::deterministic`].
    NoiseF32 { mode: NoiseMode, freq: f32, off: [f32; 2], param: f32 },
    FractalF32 { mode: NoiseMode, octaves: u32, base: f32, lac: f32, per: f32, off: [f32; 2], param: f32 },
    Abs(CIn),
    ScaleBias(CIn, Fx, Fx),
    Clamp(CIn, Fx, Fx),
    /// (input, steps, 1/steps).
    Terrace(CIn, Fx, Fx),
    Add(CIn, CIn),
    Sub(CIn, CIn),
    Mul(CIn, CIn),
    Min(CIn, CIn),
    Max(CIn, CIn),
    Lerp(CIn, CIn, CIn),
    DomainWarp(CIn, CIn, CIn, Fx),
}

#[derive(Debug, Clone, Copy)]
pub struct CompiledCaves {
    pub freq: Fx,
    pub threshold: Fx,
}

/// The deterministic evaluator: everything is Q16.16, every operation has a
/// WGSL mirror, so CPU (client + server) and GPU produce identical bits.
/// World coordinates enter as Fx (wrapping past ±32767 voxels — deterministic
/// on all ends, see `fx` module docs).
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    nodes: Vec<CKind>,
    height: CIn,
    rock: Option<CIn>,
    structure: Option<CIn>,
    pub caves: Option<CompiledCaves>,
}

impl CompiledGraph {
    /// Surface channels at a world column, exact Q16.16.
    pub fn columns_fx(&self, seed: u32, x: Fx, z: Fx) -> (Fx, Fx, Fx) {
        (
            self.eval_in(seed, self.height, x, z),
            self.rock.map_or(0, |s| self.eval_in(seed, s, x, z)),
            self.structure.map_or(0, |s| self.eval_in(seed, s, x, z)),
        )
    }

    /// [`Self::columns_fx`] converted for preview/display consumers.
    pub fn eval_columns(&self, seed: u32, x: Fx, z: Fx) -> ColumnSample {
        let (h, r, s) = self.columns_fx(seed, x, z);
        ColumnSample {
            height: fx::to_f32(h) as f64,
            rock: fx::to_f32(r) as f64,
            structure: fx::to_f32(s) as f64,
        }
    }

    /// One node's field (design-tool previews of intermediate nodes).
    pub fn node_fx(&self, seed: u32, node: NodeId, x: Fx, z: Fx) -> Fx {
        self.eval_node(seed, node, x, z)
    }

    /// True if a cave carves air at integer world voxel (x, y, z).
    pub fn cave_carves(&self, seed: u32, x: i32, y: i32, z: i32) -> bool {
        match self.caves {
            None => false,
            Some(c) => fx::abs(self.cave_noise(seed, x, y, z)) > c.threshold,
        }
    }

    /// The signed cave field at integer world voxel (x, y, z) — exposed so the
    /// chunk generator can lattice-sample + interpolate it.
    pub fn cave_noise(&self, seed: u32, x: i32, y: i32, z: i32) -> Fx {
        let c = self.caves.expect("caller checks caves");
        noise_det::noise3(
            seed,
            fx::int_mul(x, c.freq),
            fx::int_mul(y, c.freq),
            fx::int_mul(z, c.freq),
        )
    }

    fn eval_in(&self, seed: u32, slot: CIn, x: Fx, z: Fx) -> Fx {
        match slot {
            CIn::Node(id) => self.eval_node(seed, id, x, z),
            CIn::Const(v) => v,
        }
    }

    fn eval_node(&self, seed: u32, id: NodeId, x: Fx, z: Fx) -> Fx {
        let ev = |slot: CIn, x: Fx, z: Fx| self.eval_in(seed, slot, x, z);
        match &self.nodes[id] {
            CKind::Constant(v) => *v,
            CKind::Coord(Axis::X) => x,
            CKind::Coord(Axis::Z) => z,
            CKind::Noise2 { freq, off } => noise_det::noise2(
                seed,
                fx::mul(x, *freq).wrapping_add(off[0]),
                fx::mul(z, *freq).wrapping_add(off[1]),
            ),
            CKind::Fbm { octaves, base, lac, per, off } => {
                let mut f = *base;
                let mut amp = fx::ONE;
                let mut sum: Fx = 0;
                for _ in 0..*octaves {
                    let n = noise_det::noise2(
                        seed,
                        fx::mul(x, f).wrapping_add(off[0]),
                        fx::mul(z, f).wrapping_add(off[1]),
                    );
                    sum = sum.wrapping_add(fx::mul(amp, n));
                    f = fx::mul(f, *lac);
                    amp = fx::mul(amp, *per);
                }
                sum
            }
            // f32 boundary: convert the Q16.16 coordinate exactly (power-of-two
            // divide), evaluate the ported mode, quantize the result back. The
            // WGSL mirror does the same conversions — see `wgsl::HASH_NOISE`.
            CKind::NoiseF32 { mode, freq, off, param } => {
                let px = fx::to_f32(x) * freq + off[0];
                let pz = fx::to_f32(z) * freq + off[1];
                fx::from_f32(crate::noise_modes::eval_mode(*mode, px, pz, *param))
            }
            CKind::FractalF32 { mode, octaves, base, lac, per, off, param } => {
                let (xf, zf) = (fx::to_f32(x), fx::to_f32(z));
                let mut f = *base;
                let mut amp = 1.0f32;
                let mut sum = 0.0f32;
                for _ in 0..*octaves {
                    sum += amp
                        * crate::noise_modes::eval_mode(*mode, xf * f + off[0], zf * f + off[1], *param);
                    f *= lac;
                    amp *= per;
                }
                fx::from_f32(sum)
            }
            CKind::Abs(i) => fx::abs(ev(*i, x, z)),
            CKind::ScaleBias(i, s, b) => fx::mul(ev(*i, x, z), *s).wrapping_add(*b),
            CKind::Clamp(i, lo, hi) => fx::clamp(ev(*i, x, z), *lo, *hi),
            CKind::Terrace(i, s, inv_s) => {
                let r = fx::round(fx::mul(ev(*i, x, z), *s));
                fx::mul(r.wrapping_shl(16), *inv_s)
            }
            CKind::Add(a, b) => ev(*a, x, z).wrapping_add(ev(*b, x, z)),
            CKind::Sub(a, b) => ev(*a, x, z).wrapping_sub(ev(*b, x, z)),
            CKind::Mul(a, b) => fx::mul(ev(*a, x, z), ev(*b, x, z)),
            CKind::Min(a, b) => ev(*a, x, z).min(ev(*b, x, z)),
            CKind::Max(a, b) => ev(*a, x, z).max(ev(*b, x, z)),
            CKind::Lerp(a, b, t) => {
                let (va, vb) = (ev(*a, x, z), ev(*b, x, z));
                let tt = fx::clamp(ev(*t, x, z), 0, fx::ONE);
                fx::lerp(va, vb, tt)
            }
            CKind::DomainWarp(input, wx, wz, amount) => {
                let nx = x.wrapping_add(fx::mul(ev(*wx, x, z), *amount));
                let nz = z.wrapping_add(fx::mul(ev(*wz, x, z), *amount));
                ev(*input, nx, nz)
            }
        }
    }
}

impl NodeKind {
    /// The input slots this node reads, for validation / graph walks.
    pub fn inputs(&self) -> Vec<&In> {
        match self {
            NodeKind::Constant { .. }
            | NodeKind::Coord { .. }
            | NodeKind::Simplex2 { .. }
            | NodeKind::Fbm { .. }
            | NodeKind::Noise { .. }
            | NodeKind::FractalNoise { .. }
            | NodeKind::RadialFalloff { .. } => vec![],
            NodeKind::Abs { input }
            | NodeKind::ScaleBias { input, .. }
            | NodeKind::Clamp { input, .. }
            | NodeKind::Power { input, .. }
            | NodeKind::Terrace { input, .. } => vec![input],
            NodeKind::Add { a, b }
            | NodeKind::Sub { a, b }
            | NodeKind::Mul { a, b }
            | NodeKind::Min { a, b }
            | NodeKind::Max { a, b } => vec![a, b],
            NodeKind::Lerp { a, b, t } => vec![a, b, t],
            NodeKind::DomainWarp { input, wx, wz, .. } => vec![input, wx, wz],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default graph reproduces the original formula *shape*: five height
    /// octaves summed with the original amplitudes, rock as the three-term
    /// mix. With the v2 noise the values differ from the old f64 port, but the
    /// structure must hold: height centered near 256, well inside the ±115
    /// octave envelope; rock bounded by its ±30 envelope.
    #[test]
    fn default_graph_height_and_rock_envelopes() {
        let graph = TerrainGraph::default_soils();
        graph.validate().unwrap();
        let c = graph.compile().unwrap();
        let mut min_h = f64::MAX;
        let mut max_h = f64::MIN;
        for i in 0..2000u32 {
            let x = (crate::noise_det::pcg(i) % 40000) as i32 - 20000;
            let z = (crate::noise_det::pcg(i ^ 0xffff) % 40000) as i32 - 20000;
            let s = c.eval_columns(7, x.wrapping_shl(16), z.wrapping_shl(16));
            min_h = min_h.min(s.height);
            max_h = max_h.max(s.height);
            assert!(s.rock <= 5.0 && s.rock >= -30.0, "rock out of envelope: {}", s.rock);
        }
        assert!(min_h > 256.0 - 115.0 && max_h < 256.0 + 115.0, "height envelope: {min_h}..{max_h}");
        // The terrain actually varies.
        assert!(max_h - min_h > 20.0, "terrain suspiciously flat: {min_h}..{max_h}");
    }

    #[test]
    fn cave_carve_matches_noise3_directly() {
        let graph = TerrainGraph::default_soils();
        let c = graph.compile().unwrap();
        let caves = c.caves.expect("default graph has caves");
        for &(gx, gy, gz) in &[(10, 20, 30), (-5, 100, 42)] {
            let n = crate::noise_det::noise3(
                7,
                fx::int_mul(gx, caves.freq),
                fx::int_mul(gy, caves.freq),
                fx::int_mul(gz, caves.freq),
            );
            assert_eq!(c.cave_carves(7, gx, gy, gz), fx::abs(n) > caves.threshold);
        }
    }

    #[test]
    fn power_and_radial_falloff_are_rejected() {
        let mut graph = TerrainGraph::default_soils();
        let id = graph.nodes.len();
        graph.nodes.push(Node {
            id,
            kind: NodeKind::Power { input: In::constant(2.0), exponent: 2.0 },
        });
        assert!(graph.validate().is_err());
    }

    #[test]
    fn round_trips_through_ron() {
        let graph = TerrainGraph::default_soils();
        let text = ron::ser::to_string_pretty(&graph, ron::ser::PrettyConfig::default()).unwrap();
        let back: TerrainGraph = ron::from_str(&text).unwrap();
        back.validate().unwrap();
        let a = graph.eval_columns(99, 12.0, 34.0);
        let b = back.eval_columns(99, 12.0, 34.0);
        assert_eq!(a.height, b.height);
        assert_eq!(a.rock, b.rock);
    }

    /// `field_at` on the node feeding the Height output equals the Height
    /// channel — so a tool can preview intermediate nodes with the same math.
    #[test]
    fn field_at_height_node_matches_eval_columns() {
        let graph = TerrainGraph::default_soils();
        let height_node = graph.outputs.height.node.expect("default height is wired");
        for &(x, z) in &[(0.0, 0.0), (55.0, -120.0), (900.0, 410.0)] {
            let via_field = graph.field_at(2024, height_node, x, z);
            let via_columns = graph.eval_columns(2024, x, z).height;
            assert_eq!(via_field, via_columns, "mismatch at ({x}, {z})");
        }
    }
}
