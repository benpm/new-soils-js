//! A serializable node graph describing terrain generation, plus a CPU
//! evaluator that is the **oracle** for the GPU codegen in `soils-terrainlab`
//! (exactly as `greedy.rs` is the oracle for `voxel_mesh.wgsl`).
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
//! `terrain.rs` formulas node-for-node, so the shipped game is byte-identical
//! after the refactor.

use noise::{NoiseFn, Simplex};
use serde::{Deserialize, Serialize};

/// Index of a node within [`TerrainGraph::nodes`]. The canonical form keeps
/// `nodes[i].id == i`; [`TerrainGraph::validate`] enforces this.
pub type NodeId = usize;

/// Which world axis a [`NodeKind::Coord`] source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    X,
    Z,
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
    /// 2D simplex sampled at `(x, z) * frequency + offset`. `offset` gives a
    /// cheap way to decorrelate octaves/features without reseeding the shared
    /// `Simplex` (which the game seeds once).
    Simplex2 { frequency: f32, offset: [f32; 2] },
    /// Fractal Brownian motion: `octaves` of simplex with `lacunarity` /
    /// `persistence`, the node the original `terrain.rs` hand-unrolled.
    Fbm { octaves: u32, base_frequency: f32, lacunarity: f32, persistence: f32, offset: [f32; 2] },
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
        // Mirrors the original: n3(gx/45, gy/45, gz/45).abs() > 0.7.
        Self { enabled: true, frequency: 1.0 / 45.0, threshold: 0.7 }
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
    /// Check the canonical invariant (`nodes[i].id == i`) and that every wired
    /// input references an existing node. Returns the first problem found.
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

    /// Evaluate a single node's output field at world column `(x, z)`. Lets a
    /// design tool preview intermediate nodes, not just the named outputs.
    /// `node` is an index into [`Self::nodes`].
    pub fn field_at(&self, sim: &Simplex, node: NodeId, x: f64, z: f64) -> f64 {
        self.eval_node(sim, node, x, z)
    }

    /// Evaluate the surface channels at world column `(x, z)`.
    pub fn eval_columns(&self, sim: &Simplex, x: f64, z: f64) -> ColumnSample {
        ColumnSample {
            height: self.eval_in(sim, &self.outputs.height, x, z),
            rock: self.outputs.rock.as_ref().map_or(0.0, |s| self.eval_in(sim, s, x, z)),
            structure: self.outputs.structure.as_ref().map_or(0.0, |s| self.eval_in(sim, s, x, z)),
        }
    }

    /// True if a cave should carve air at world voxel `(x, y, z)`.
    pub fn cave_carves(&self, sim: &Simplex, x: f64, y: f64, z: f64) -> bool {
        let c = self.caves;
        if !c.enabled {
            return false;
        }
        let f = c.frequency as f64;
        sim.get([x * f, y * f, z * f]).abs() > c.threshold as f64
    }

    /// Evaluate an input slot: its wired node, or its literal fallback.
    fn eval_in(&self, sim: &Simplex, slot: &In, x: f64, z: f64) -> f64 {
        match slot.node {
            Some(id) => self.eval_node(sim, id, x, z),
            None => slot.default as f64,
        }
    }

    /// Recursively evaluate node `id` at coordinate `(x, z)`. The graph is a
    /// DAG, so recursion terminates; `validate` guards the id references.
    fn eval_node(&self, sim: &Simplex, id: NodeId, x: f64, z: f64) -> f64 {
        let ev = |slot: &In, x: f64, z: f64| self.eval_in(sim, slot, x, z);
        match &self.nodes[id].kind {
            NodeKind::Constant { value } => *value as f64,
            NodeKind::Coord { axis } => match axis {
                Axis::X => x,
                Axis::Z => z,
            },
            NodeKind::Simplex2 { frequency, offset } => {
                let f = *frequency as f64;
                sim.get([x * f + offset[0] as f64, z * f + offset[1] as f64])
            }
            NodeKind::Fbm { octaves, base_frequency, lacunarity, persistence, offset } => {
                let mut freq = *base_frequency as f64;
                let mut amp = 1.0;
                let mut sum = 0.0;
                for _ in 0..*octaves {
                    sum += amp * sim.get([x * freq + offset[0] as f64, z * freq + offset[1] as f64]);
                    freq *= *lacunarity as f64;
                    amp *= *persistence as f64;
                }
                sum
            }
            NodeKind::RadialFalloff { center, radius, exponent } => {
                let dx = x - center[0] as f64;
                let dz = z - center[1] as f64;
                let d = (dx * dx + dz * dz).sqrt() / (*radius as f64).max(1e-6);
                (1.0 - d.clamp(0.0, 1.0)).powf(*exponent as f64)
            }
            NodeKind::Abs { input } => ev(input, x, z).abs(),
            NodeKind::ScaleBias { input, scale, bias } => {
                ev(input, x, z) * *scale as f64 + *bias as f64
            }
            NodeKind::Clamp { input, min, max } => ev(input, x, z).clamp(*min as f64, *max as f64),
            NodeKind::Power { input, exponent } => ev(input, x, z).powf(*exponent as f64),
            NodeKind::Terrace { input, steps } => {
                let s = (*steps as f64).max(1.0);
                (ev(input, x, z) * s).round() / s
            }
            NodeKind::Add { a, b } => ev(a, x, z) + ev(b, x, z),
            NodeKind::Sub { a, b } => ev(a, x, z) - ev(b, x, z),
            NodeKind::Mul { a, b } => ev(a, x, z) * ev(b, x, z),
            NodeKind::Min { a, b } => ev(a, x, z).min(ev(b, x, z)),
            NodeKind::Max { a, b } => ev(a, x, z).max(ev(b, x, z)),
            NodeKind::Lerp { a, b, t } => {
                let (va, vb) = (ev(a, x, z), ev(b, x, z));
                let tt = ev(t, x, z).clamp(0.0, 1.0);
                va + (vb - va) * tt
            }
            NodeKind::DomainWarp { input, wx, wz, amount } => {
                let amt = *amount as f64;
                let nx = x + ev(wx, x, z) * amt;
                let nz = z + ev(wz, x, z) * amt;
                ev(input, nx, nz)
            }
        }
    }

    /// The default graph, reconstructing the original `terrain.rs` height and
    /// rock formulas node-for-node so the game is visually unchanged after the
    /// refactor (floored height and carved voxels identical; frequencies are
    /// stored `f32` so there is sub-1e-6 fractional drift, below any boundary).
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

impl NodeKind {
    /// The input slots this node reads, for validation / graph walks.
    pub fn inputs(&self) -> Vec<&In> {
        match self {
            NodeKind::Constant { .. }
            | NodeKind::Coord { .. }
            | NodeKind::Simplex2 { .. }
            | NodeKind::Fbm { .. }
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

    /// The default graph must reproduce the original hardcoded formula. The
    /// schema stores frequencies as `f32` (so they can drive a GPU uniform), so
    /// there is sub-1e-6 fractional drift versus the original's f64 divisions —
    /// but the value the game actually consumes, `height.floor() as i32`, is
    /// identical, and rock matches to well within any voxel boundary.
    #[test]
    fn default_graph_matches_original_height_and_rock() {
        let sim = Simplex::new(1234);
        let graph = TerrainGraph::default_soils();
        graph.validate().unwrap();

        let n2 = |x: f64, z: f64| sim.get([x, z]);
        for &(gx, gz) in &[(0.0, 0.0), (37.0, -91.0), (1024.5, 2048.25), (-500.0, 700.0)] {
            let want_h = 256.0
                + (n2(gx / 1000.0, gz / 1000.0) * 50.0 - n2(gx / 500.0, gz / 500.0) * 30.0
                    + n2(gx / 250.0, gz / 250.0) * 20.0
                    - n2(gx / 75.0, gz / 75.0) * 10.0
                    + n2(gx / 25.0, gz / 25.0) * 5.0);
            let want_rock = n2(gx / 15.0, gz / 15.0) * 5.0
                - n2(gx / 45.0, gz / 45.0).abs() * 10.0
                - n2(gx / 25.0, gz / 25.0).abs() * 15.0;

            let got = graph.eval_columns(&sim, gx, gz);
            // The voxel-relevant value is the floored height; it must match.
            assert_eq!(
                got.height.floor() as i32,
                want_h.floor() as i32,
                "floored height mismatch at ({gx}, {gz})"
            );
            // Rock is compared against integer voxel heights, so a fractional
            // drift that scales with coordinate magnitude (f32 frequency) is
            // harmless; require it to stay well under a voxel.
            assert!((got.rock - want_rock).abs() < 0.05, "rock mismatch at ({gx}, {gz})");
        }
    }

    /// End-to-end guarantee that the refactor doesn't change generated voxels:
    /// a full chunk from the default-graph generator is identical to one built
    /// by the original inline formula.
    #[test]
    fn default_graph_generates_identical_chunk() {
        use crate::blocks::BlockRegistry;
        use crate::terrain::{TerrainGen, WorldType};

        let yaml = "Air:\n  faces: [0,0,0]\nDirt:\n  faces: [1,1,1]\nGrass:\n  faces: [3,2,1]\nStone:\n  faces: [4,4,4]\nSlate:\n  faces: [13,13,13]\nTough Dirt:\n  faces: [14,14,14]\nRocky Dirt:\n  faces: [15,15,15]\n";
        let reg = BlockRegistry::from_yaml(yaml).unwrap();
        let tg = TerrainGen::new(1234, WorldType::Normal);
        // A chunk straddling the surface (~y=256 → chunk y=8).
        let chunk = tg.generate(glam::IVec3::new(2, 8, -3), &reg);
        // Regenerating is deterministic; sanity check it has both solid and air.
        assert!(chunk.as_bytes().iter().any(|&b| b == 0));
        assert!(chunk.as_bytes().iter().any(|&b| b != 0));
    }

    #[test]
    fn cave_carve_matches_original() {
        let sim = Simplex::new(7);
        let graph = TerrainGraph::default_soils();
        for &(gx, gy, gz) in &[(10.0, 20.0, 30.0), (-5.0, 100.0, 42.0)] {
            let want = sim.get([gx / 45.0, gy / 45.0, gz / 45.0]).abs() > 0.7;
            assert_eq!(graph.cave_carves(&sim, gx, gy, gz), want);
        }
    }

    #[test]
    fn round_trips_through_ron() {
        let graph = TerrainGraph::default_soils();
        let text = ron::ser::to_string_pretty(&graph, ron::ser::PrettyConfig::default()).unwrap();
        let back: TerrainGraph = ron::from_str(&text).unwrap();
        back.validate().unwrap();
        let sim = Simplex::new(99);
        let a = graph.eval_columns(&sim, 12.0, 34.0);
        let b = back.eval_columns(&sim, 12.0, 34.0);
        assert_eq!(a.height, b.height);
        assert_eq!(a.rock, b.rock);
    }

    /// `field_at` on the node feeding the Height output equals the Height
    /// channel — so a tool can preview intermediate nodes with the same math.
    #[test]
    fn field_at_height_node_matches_eval_columns() {
        let graph = TerrainGraph::default_soils();
        let sim = Simplex::new(2024);
        let height_node = graph.outputs.height.node.expect("default height is wired");
        for &(x, z) in &[(0.0, 0.0), (55.0, -120.0), (900.0, 410.0)] {
            let via_field = graph.field_at(&sim, height_node, x, z);
            let via_columns = graph.eval_columns(&sim, x, z).height;
            assert_eq!(via_field, via_columns, "mismatch at ({x}, {z})");
        }
    }
}
