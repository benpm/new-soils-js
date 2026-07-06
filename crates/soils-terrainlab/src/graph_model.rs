//! The editor's own node-graph model and its conversion to/from the
//! serializable [`TerrainGraph`].
//!
//! We render the node canvas by hand on top of `egui::Scene` (see
//! [`crate::canvas`]) rather than using a node-graph crate: the only
//! egui-0.33-compatible `egui-snarl` release does not composite its nodes under
//! `bevy_egui 0.39` (its transformed sublayer renders blank), whereas the
//! built-in `egui::Scene` — same transform mechanism — works. So this module
//! owns graph *data* (nodes + positions + wires) and the lowering to the game's
//! schema; `canvas` owns interaction.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui::{Pos2, Rect, Vec2};
use soils_worldgen::graph::{In, Node, NodeKind, Outputs, TerrainGraph};

use crate::node::{EditorNode, OutChannel};

/// Column / row spacing used by the layered layout (matches `from_terrain_graph`).
const COL_W: f32 = 210.0;
const ROW_H: f32 = 150.0;
/// Approximate on-canvas node extent, for `bounds()` framing.
const NODE_EXTENT: Vec2 = Vec2::new(176.0, 120.0);

/// A placed node: its kind (parameters) and canvas position. `PartialEq` backs
/// the undo/redo change detection.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInst {
    pub kind: EditorNode,
    pub pos: Pos2,
}

/// A directed connection from a node's single output to a specific input pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    pub from: usize,
    pub to: usize,
    pub input: usize,
}

/// The full editor graph: nodes indexed by position, plus wires. Kept minimal
/// and `egui`-only so it round-trips to `TerrainGraph` without a node-graph dep.
/// `PartialEq` lets undo/redo compare two graph states.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorGraph {
    pub nodes: Vec<NodeInst>,
    pub wires: Vec<Wire>,
}

/// Why an editor graph couldn't be lowered to a `TerrainGraph`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    Cycle,
    NoHeightOutput,
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::Cycle => write!(f, "graph has a cycle"),
            ConvertError::NoHeightOutput => write!(f, "no Height output is connected"),
        }
    }
}

impl EditorGraph {
    /// Add a node at `pos`, returning its index.
    pub fn add(&mut self, kind: EditorNode, pos: Pos2) -> usize {
        self.nodes.push(NodeInst { kind, pos });
        self.nodes.len() - 1
    }

    /// Remove a node and any wires touching it, renumbering remaining wire
    /// endpoints so indices stay valid.
    pub fn remove(&mut self, idx: usize) {
        self.nodes.remove(idx);
        self.wires.retain(|w| w.from != idx && w.to != idx);
        for w in &mut self.wires {
            if w.from > idx {
                w.from -= 1;
            }
            if w.to > idx {
                w.to -= 1;
            }
        }
    }

    /// Connect `from`'s output to (`to`, `input`), replacing any existing wire
    /// into that input pin (each input takes one wire).
    pub fn connect(&mut self, from: usize, to: usize, input: usize) {
        if from == to {
            return;
        }
        self.wires.retain(|w| !(w.to == to && w.input == input));
        self.wires.push(Wire { from, to, input });
    }

    /// The source node feeding (`to`, `input`), if any.
    pub fn source_of(&self, to: usize, input: usize) -> Option<usize> {
        self.wires.iter().find(|w| w.to == to && w.input == input).map(|w| w.from)
    }

    /// Re-place every node in a tidy left→right layered layout: a node's column
    /// is its longest-path depth from a source, rows fill top-down within a
    /// column. Nodes and wires are preserved (only positions change). Output
    /// sinks, having the deepest inputs, land in the rightmost column.
    pub fn auto_layout(&mut self) {
        let depth = self.node_depths();
        let mut row_in_col: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
        for (i, &d) in depth.iter().enumerate() {
            let row = row_in_col.entry(d).or_insert(0);
            self.nodes[i].pos = Pos2::new(d as f32 * COL_W, *row as f32 * ROW_H);
            *row += 1;
        }
    }

    /// Longest path (in edges) from any source to each node, over the editor
    /// wires. Cycle-guarded (`depth[i] = 0` before recursing) so a transient
    /// editing cycle can't loop forever.
    fn node_depths(&self) -> Vec<i32> {
        fn go(g: &EditorGraph, i: usize, depth: &mut [i32]) -> i32 {
            if depth[i] >= 0 {
                return depth[i];
            }
            depth[i] = 0;
            let mut d = 0;
            for w in &g.wires {
                if w.to == i {
                    d = d.max(1 + go(g, w.from, depth));
                }
            }
            depth[i] = d;
            d
        }
        let mut depth = vec![-1i32; self.nodes.len()];
        for i in 0..self.nodes.len() {
            go(self, i, &mut depth);
        }
        depth
    }

    /// Bounding rect over node positions (plus node extent), for view framing.
    pub fn bounds(&self) -> Rect {
        if self.nodes.is_empty() {
            return Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0));
        }
        let mut r = Rect::NOTHING;
        for node in &self.nodes {
            r.extend_with(node.pos);
            r.extend_with(node.pos + NODE_EXTENT);
        }
        r
    }

    /// Per editor node, its index in the lowered [`TerrainGraph`] (`None` for
    /// `Output` sinks, which are dropped). Mirrors `to_terrain_graph`'s
    /// canonical numbering, so it can address a node in `state.graph`.
    pub fn canonical_map(&self) -> Vec<Option<usize>> {
        let mut map = vec![None; self.nodes.len()];
        let mut next = 0;
        for (i, node) in self.nodes.iter().enumerate() {
            if !matches!(node.kind, EditorNode::Output { .. }) {
                map[i] = Some(next);
                next += 1;
            }
        }
        map
    }

    /// A content signature per node: a Merkle hash of `(seed, own params,
    /// input signatures)`. A node's signature changes iff its own parameters or
    /// anything upstream changed — i.e. exactly when its output field would —
    /// so it doubles as a content-addressed cache key for per-node previews and
    /// is stable across index renumbering. Cycle-guarded (revisit → 0).
    pub fn node_signatures(&self, seed: u32) -> Vec<u64> {
        let n = self.nodes.len();
        let mut sig = vec![0u64; n];
        let mut done = vec![false; n];
        let mut visiting = vec![false; n];
        for i in 0..n {
            self.sig_of(i, seed, &mut sig, &mut done, &mut visiting);
        }
        sig
    }

    fn sig_of(
        &self,
        i: usize,
        seed: u32,
        sig: &mut [u64],
        done: &mut [bool],
        visiting: &mut [bool],
    ) -> u64 {
        if done[i] {
            return sig[i];
        }
        if visiting[i] {
            return 0; // transient cycle: break it
        }
        visiting[i] = true;
        let mut h = DefaultHasher::new();
        seed.hash(&mut h);
        // Debug is a stable, exhaustive rendering of the kind incl. f32 params.
        format!("{:?}", self.nodes[i].kind).hash(&mut h);
        for k in 0..self.nodes[i].kind.input_count() {
            let child = match self.source_of(i, k) {
                Some(src) => self.sig_of(src, seed, sig, done, visiting),
                None => 0,
            };
            child.hash(&mut h);
        }
        let s = h.finish();
        visiting[i] = false;
        done[i] = true;
        sig[i] = s;
        s
    }

    /// Lower to a `TerrainGraph`. `Err` on a cycle or missing Height output.
    pub fn to_terrain_graph(&self) -> Result<TerrainGraph, ConvertError> {
        // Canonical index for each value (non-Output) node.
        let mut canonical = vec![usize::MAX; self.nodes.len()];
        let mut next = 0;
        for (i, n) in self.nodes.iter().enumerate() {
            if !matches!(n.kind, EditorNode::Output { .. }) {
                canonical[i] = next;
                next += 1;
            }
        }

        let slot = |to: usize, input: usize| -> In {
            match self.source_of(to, input) {
                Some(src) if canonical[src] != usize::MAX => In::from(canonical[src]),
                _ => In::constant(0.0),
            }
        };

        let mut nodes: Vec<Node> = Vec::with_capacity(next);
        for (i, inst) in self.nodes.iter().enumerate() {
            if canonical[i] == usize::MAX {
                continue;
            }
            let kind = build_kind(&inst.kind, |input| slot(i, input));
            nodes.push(Node { id: canonical[i], kind });
        }

        let (mut height, mut rock, mut structure) = (None, None, None);
        for (i, inst) in self.nodes.iter().enumerate() {
            if let EditorNode::Output { channel } = inst.kind {
                let s = slot(i, 0);
                if s.node.is_none() {
                    continue;
                }
                match channel {
                    OutChannel::Height => height = Some(s),
                    OutChannel::Rock => rock = Some(s),
                    OutChannel::Structure => structure = Some(s),
                }
            }
        }
        let height = height.ok_or(ConvertError::NoHeightOutput)?;

        let graph = TerrainGraph {
            nodes,
            outputs: Outputs { height, rock, structure },
            caves: soils_worldgen::graph::CaveParams::default(),
        };
        if has_cycle(&graph) {
            return Err(ConvertError::Cycle);
        }
        Ok(graph)
    }

    /// Build an editor graph from a `TerrainGraph`, laying nodes out left-to-
    /// right by evaluation depth and reconnecting wires + output sinks.
    pub fn from_terrain_graph(graph: &TerrainGraph) -> Self {
        let depth = longest_depths(graph);
        let out_col = depth.iter().copied().max().unwrap_or(0) + 1;
        let mut eg = EditorGraph::default();
        let mut row_in_col: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
        let place = |row_in_col: &mut std::collections::HashMap<i32, i32>, col: i32| -> Pos2 {
            let row = row_in_col.entry(col).or_insert(0);
            let p = Pos2::new(col as f32 * COL_W, *row as f32 * ROW_H);
            *row += 1;
            p
        };

        // Value nodes: canonical index i maps directly to editor index i.
        for node in &graph.nodes {
            let pos = place(&mut row_in_col, depth[node.id]);
            eg.add(editor_from_kind(&node.kind), pos);
        }
        // Value-node wires.
        for node in &graph.nodes {
            for (input, s) in node.kind.inputs().iter().enumerate() {
                if let Some(src) = s.node {
                    eg.wires.push(Wire { from: src, to: node.id, input });
                }
            }
        }
        // Output sinks.
        let add_out = |eg: &mut EditorGraph, ch: OutChannel, s: &In, rc: &mut std::collections::HashMap<i32, i32>| {
            let pos = place(rc, out_col);
            let idx = eg.add(EditorNode::Output { channel: ch }, pos);
            if let Some(src) = s.node {
                eg.wires.push(Wire { from: src, to: idx, input: 0 });
            }
        };
        add_out(&mut eg, OutChannel::Height, &graph.outputs.height, &mut row_in_col);
        if let Some(s) = &graph.outputs.rock {
            add_out(&mut eg, OutChannel::Rock, s, &mut row_in_col);
        }
        if let Some(s) = &graph.outputs.structure {
            add_out(&mut eg, OutChannel::Structure, s, &mut row_in_col);
        }
        eg
    }
}

fn build_kind(node: &EditorNode, input: impl Fn(usize) -> In) -> NodeKind {
    match *node {
        EditorNode::Constant { value } => NodeKind::Constant { value },
        EditorNode::Coord { axis } => NodeKind::Coord { axis },
        EditorNode::Simplex2 { frequency, offset } => NodeKind::Simplex2 { frequency, offset },
        EditorNode::Fbm { octaves, base_frequency, lacunarity, persistence, offset } => {
            NodeKind::Fbm { octaves, base_frequency, lacunarity, persistence, offset }
        }
        EditorNode::RadialFalloff { center, radius, exponent } => {
            NodeKind::RadialFalloff { center, radius, exponent }
        }
        EditorNode::Abs => NodeKind::Abs { input: input(0) },
        EditorNode::ScaleBias { scale, bias } => NodeKind::ScaleBias { input: input(0), scale, bias },
        EditorNode::Clamp { min, max } => NodeKind::Clamp { input: input(0), min, max },
        EditorNode::Power { exponent } => NodeKind::Power { input: input(0), exponent },
        EditorNode::Terrace { steps } => NodeKind::Terrace { input: input(0), steps },
        EditorNode::Add => NodeKind::Add { a: input(0), b: input(1) },
        EditorNode::Sub => NodeKind::Sub { a: input(0), b: input(1) },
        EditorNode::Mul => NodeKind::Mul { a: input(0), b: input(1) },
        EditorNode::Min => NodeKind::Min { a: input(0), b: input(1) },
        EditorNode::Max => NodeKind::Max { a: input(0), b: input(1) },
        EditorNode::Lerp => NodeKind::Lerp { a: input(0), b: input(1), t: input(2) },
        EditorNode::DomainWarp { amount } => {
            NodeKind::DomainWarp { input: input(0), wx: input(1), wz: input(2), amount }
        }
        EditorNode::Output { .. } => NodeKind::Constant { value: 0.0 },
    }
}

fn editor_from_kind(kind: &NodeKind) -> EditorNode {
    match *kind {
        NodeKind::Constant { value } => EditorNode::Constant { value },
        NodeKind::Coord { axis } => EditorNode::Coord { axis },
        NodeKind::Simplex2 { frequency, offset } => EditorNode::Simplex2 { frequency, offset },
        NodeKind::Fbm { octaves, base_frequency, lacunarity, persistence, offset } => {
            EditorNode::Fbm { octaves, base_frequency, lacunarity, persistence, offset }
        }
        NodeKind::RadialFalloff { center, radius, exponent } => {
            EditorNode::RadialFalloff { center, radius, exponent }
        }
        NodeKind::Abs { .. } => EditorNode::Abs,
        NodeKind::ScaleBias { scale, bias, .. } => EditorNode::ScaleBias { scale, bias },
        NodeKind::Clamp { min, max, .. } => EditorNode::Clamp { min, max },
        NodeKind::Power { exponent, .. } => EditorNode::Power { exponent },
        NodeKind::Terrace { steps, .. } => EditorNode::Terrace { steps },
        NodeKind::Add { .. } => EditorNode::Add,
        NodeKind::Sub { .. } => EditorNode::Sub,
        NodeKind::Mul { .. } => EditorNode::Mul,
        NodeKind::Min { .. } => EditorNode::Min,
        NodeKind::Max { .. } => EditorNode::Max,
        NodeKind::Lerp { .. } => EditorNode::Lerp,
        NodeKind::DomainWarp { amount, .. } => EditorNode::DomainWarp { amount },
    }
}

/// DFS 3-colour cycle check over the resolved index graph.
fn has_cycle(graph: &TerrainGraph) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum C {
        White,
        Gray,
        Black,
    }
    let n = graph.nodes.len();
    let mut color = vec![C::White; n];
    let succ = |i: usize| -> Vec<usize> {
        graph.nodes[i].kind.inputs().into_iter().filter_map(|s| s.node).collect()
    };
    for start in 0..n {
        if color[start] != C::White {
            continue;
        }
        let mut stack: Vec<(usize, std::vec::IntoIter<usize>)> = vec![(start, succ(start).into_iter())];
        color[start] = C::Gray;
        while let Some((_, iter)) = stack.last_mut() {
            match iter.next() {
                Some(next) => match color[next] {
                    C::Gray => return true,
                    C::White => {
                        color[next] = C::Gray;
                        stack.push((next, succ(next).into_iter()));
                    }
                    C::Black => {}
                },
                None => {
                    let (done, _) = stack.pop().unwrap();
                    color[done] = C::Black;
                }
            }
        }
    }
    false
}

/// Longest path (edges) from any source to each node, for column layout.
fn longest_depths(graph: &TerrainGraph) -> Vec<i32> {
    let n = graph.nodes.len();
    let mut depth = vec![-1i32; n];
    fn go(graph: &TerrainGraph, i: usize, depth: &mut [i32]) -> i32 {
        if depth[i] >= 0 {
            return depth[i];
        }
        depth[i] = 0;
        let mut d = 0;
        for input in graph.nodes[i].kind.inputs() {
            if let Some(src) = input.node {
                d = d.max(1 + go(graph, src, depth));
            }
        }
        depth[i] = d;
        d
    }
    for i in 0..n {
        go(graph, i, &mut depth);
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_graph_round_trips() {
        let graph = TerrainGraph::default_soils();
        let eg = EditorGraph::from_terrain_graph(&graph);
        let back = eg.to_terrain_graph().expect("lowers");
        back.validate().unwrap();
        assert_eq!(graph.nodes.len(), back.nodes.len());
        let sim = noise::Simplex::new(7);
        for &(x, z) in &[(0.0, 0.0), (321.0, -88.0), (1500.0, 640.0)] {
            let a = graph.eval_columns(&sim, x, z);
            let b = back.eval_columns(&sim, x, z);
            assert!((a.height - b.height).abs() < 1e-6);
            assert!((a.rock - b.rock).abs() < 1e-6);
        }
    }

    #[test]
    fn missing_height_is_error() {
        let mut eg = EditorGraph::default();
        eg.add(EditorNode::Constant { value: 1.0 }, Pos2::ZERO);
        assert!(matches!(eg.to_terrain_graph(), Err(ConvertError::NoHeightOutput)));
    }

    #[test]
    fn remove_renumbers_wires() {
        let mut eg = EditorGraph::default();
        let a = eg.add(EditorNode::Simplex2 { frequency: 0.01, offset: [0.0; 2] }, Pos2::ZERO);
        let b = eg.add(EditorNode::Abs, Pos2::ZERO);
        let c = eg.add(EditorNode::Output { channel: OutChannel::Height }, Pos2::ZERO);
        eg.connect(a, b, 0);
        eg.connect(b, c, 0);
        eg.remove(a); // b->1? no: a=0 removed, b becomes 0, c becomes 1
        // Wire a->b dropped; wire b->c survives with renumbered indices.
        assert_eq!(eg.wires.len(), 1);
        assert_eq!(eg.wires[0], Wire { from: 0, to: 1, input: 0 });
    }

    #[test]
    fn auto_layout_orders_left_to_right() {
        let mut eg = EditorGraph::from_terrain_graph(&TerrainGraph::default_soils());
        let (nodes, wires) = (eg.nodes.len(), eg.wires.len());
        // Scramble positions, then tidy.
        for (i, node) in eg.nodes.iter_mut().enumerate() {
            node.pos = Pos2::new((i as f32 * 37.0) % 500.0, (i as f32 * 91.0) % 500.0);
        }
        eg.auto_layout();
        // Topology preserved.
        assert_eq!(eg.nodes.len(), nodes);
        assert_eq!(eg.wires.len(), wires);
        // Every wire flows strictly left→right (source column < target column).
        for w in &eg.wires {
            assert!(
                eg.nodes[w.from].pos.x < eg.nodes[w.to].pos.x,
                "wire {w:?} not left→right: from x={} to x={}",
                eg.nodes[w.from].pos.x,
                eg.nodes[w.to].pos.x
            );
        }
    }

    #[test]
    fn signatures_track_upstream_changes() {
        let mut eg = EditorGraph::default();
        let a = eg.add(EditorNode::Simplex2 { frequency: 0.01, offset: [0.0; 2] }, Pos2::ZERO);
        let b = eg.add(EditorNode::Simplex2 { frequency: 0.02, offset: [0.0; 2] }, Pos2::ZERO);
        let s = eg.add(EditorNode::ScaleBias { scale: 2.0, bias: 0.0 }, Pos2::ZERO);
        let out = eg.add(EditorNode::Output { channel: OutChannel::Height }, Pos2::ZERO);
        eg.connect(a, s, 0);
        eg.connect(s, out, 0);

        let before = eg.node_signatures(0);
        if let EditorNode::Simplex2 { frequency, .. } = &mut eg.nodes[a].kind {
            *frequency = 0.05;
        }
        let after = eg.node_signatures(0);

        assert_ne!(before[a], after[a], "changed leaf's sig must change");
        assert_ne!(before[s], after[s], "downstream node's sig must change");
        assert_eq!(before[b], after[b], "unrelated branch's sig unchanged");
    }

    #[test]
    fn identical_leaves_hash_equal() {
        let mut eg = EditorGraph::default();
        let a = eg.add(EditorNode::Simplex2 { frequency: 0.01, offset: [0.0; 2] }, Pos2::ZERO);
        let b = eg.add(EditorNode::Simplex2 { frequency: 0.01, offset: [0.0; 2] }, Pos2::ZERO);
        let sig = eg.node_signatures(0);
        assert_eq!(sig[a], sig[b], "identical leaves must hash equal");
    }
}
