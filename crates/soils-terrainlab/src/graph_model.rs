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

use egui::Pos2;
use soils_worldgen::graph::{In, Node, NodeKind, Outputs, TerrainGraph};

use crate::node::{EditorNode, OutChannel};

/// A placed node: its kind (parameters) and canvas position.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Default)]
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
            let p = Pos2::new(col as f32 * 210.0, *row as f32 * 150.0);
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
}
