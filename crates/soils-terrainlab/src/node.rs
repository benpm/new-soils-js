//! The editor-side node type held by [`crate::graph_model::EditorGraph`]. It
//! mirrors [`soils_worldgen::graph::NodeKind`] but stores only the node's *own*
//! parameters — wiring is owned by the `EditorGraph` and resolved into
//! [`In`](soils_worldgen::graph::In) slots by [`crate::graph_model`].

use serde::{Deserialize, Serialize};
use soils_worldgen::graph::Axis;

/// Which terrain channel an [`EditorNode::Output`] sink drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutChannel {
    Height,
    Rock,
    Structure,
}

impl OutChannel {
    pub const ALL: [OutChannel; 3] = [OutChannel::Height, OutChannel::Rock, OutChannel::Structure];
    pub fn label(self) -> &'static str {
        match self {
            OutChannel::Height => "Height",
            OutChannel::Rock => "Rock",
            OutChannel::Structure => "Structure",
        }
    }
}

/// A node as edited in the GUI. Parameter fields match the corresponding
/// `NodeKind` variant; input connections live in the Snarl, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EditorNode {
    // sources
    Constant { value: f32 },
    Coord { axis: Axis },
    Simplex2 { frequency: f32, offset: [f32; 2] },
    Fbm { octaves: u32, base_frequency: f32, lacunarity: f32, persistence: f32, offset: [f32; 2] },
    RadialFalloff { center: [f32; 2], radius: f32, exponent: f32 },
    // unary
    Abs,
    ScaleBias { scale: f32, bias: f32 },
    Clamp { min: f32, max: f32 },
    Power { exponent: f32 },
    Terrace { steps: f32 },
    // binary / ternary
    Add,
    Sub,
    Mul,
    Min,
    Max,
    Lerp,
    DomainWarp { amount: f32 },
    // sink
    Output { channel: OutChannel },
}

impl EditorNode {
    /// Node title shown in the editor.
    pub fn title(&self) -> String {
        match self {
            EditorNode::Constant { .. } => "Constant",
            EditorNode::Coord { .. } => "Coord",
            EditorNode::Simplex2 { .. } => "Simplex2",
            EditorNode::Fbm { .. } => "fBm",
            EditorNode::RadialFalloff { .. } => "Radial Falloff",
            EditorNode::Abs => "Abs",
            EditorNode::ScaleBias { .. } => "Scale + Bias",
            EditorNode::Clamp { .. } => "Clamp",
            EditorNode::Power { .. } => "Power",
            EditorNode::Terrace { .. } => "Terrace",
            EditorNode::Add => "Add",
            EditorNode::Sub => "Subtract",
            EditorNode::Mul => "Multiply",
            EditorNode::Min => "Min",
            EditorNode::Max => "Max",
            EditorNode::Lerp => "Lerp",
            EditorNode::DomainWarp { .. } => "Domain Warp",
            EditorNode::Output { channel } => return format!("Output: {}", channel.label()),
        }
        .to_owned()
    }

    /// Number of input pins.
    pub fn input_count(&self) -> usize {
        match self {
            EditorNode::Constant { .. }
            | EditorNode::Coord { .. }
            | EditorNode::Simplex2 { .. }
            | EditorNode::Fbm { .. }
            | EditorNode::RadialFalloff { .. } => 0,
            EditorNode::Abs
            | EditorNode::ScaleBias { .. }
            | EditorNode::Clamp { .. }
            | EditorNode::Power { .. }
            | EditorNode::Terrace { .. }
            | EditorNode::Output { .. } => 1,
            EditorNode::Add
            | EditorNode::Sub
            | EditorNode::Mul
            | EditorNode::Min
            | EditorNode::Max
            | EditorNode::DomainWarp { .. } => match self {
                EditorNode::DomainWarp { .. } => 3,
                _ => 2,
            },
            EditorNode::Lerp => 3,
        }
    }

    /// Number of output pins (0 for sink nodes).
    pub fn output_count(&self) -> usize {
        match self {
            EditorNode::Output { .. } => 0,
            _ => 1,
        }
    }

    /// Human labels for each input pin (for pin tooltips / layout).
    pub fn input_label(&self, i: usize) -> &'static str {
        match self {
            EditorNode::Lerp => ["a", "b", "t"].get(i).copied().unwrap_or("in"),
            EditorNode::DomainWarp { .. } => {
                ["in", "warp x", "warp z"].get(i).copied().unwrap_or("in")
            }
            EditorNode::Add | EditorNode::Sub | EditorNode::Mul | EditorNode::Min | EditorNode::Max => {
                ["a", "b"].get(i).copied().unwrap_or("in")
            }
            _ => "in",
        }
    }

    /// A fresh node of each kind, for the "add node" palette.
    pub fn palette() -> Vec<EditorNode> {
        vec![
            EditorNode::Constant { value: 0.0 },
            EditorNode::Coord { axis: Axis::X },
            EditorNode::Simplex2 { frequency: 0.01, offset: [0.0, 0.0] },
            EditorNode::Fbm {
                octaves: 4,
                base_frequency: 0.01,
                lacunarity: 2.0,
                persistence: 0.5,
                offset: [0.0, 0.0],
            },
            EditorNode::RadialFalloff { center: [0.0, 0.0], radius: 512.0, exponent: 2.0 },
            EditorNode::Abs,
            EditorNode::ScaleBias { scale: 1.0, bias: 0.0 },
            EditorNode::Clamp { min: 0.0, max: 1.0 },
            EditorNode::Power { exponent: 2.0 },
            EditorNode::Terrace { steps: 8.0 },
            EditorNode::Add,
            EditorNode::Sub,
            EditorNode::Mul,
            EditorNode::Min,
            EditorNode::Max,
            EditorNode::Lerp,
            EditorNode::DomainWarp { amount: 50.0 },
            EditorNode::Output { channel: OutChannel::Height },
            EditorNode::Output { channel: OutChannel::Structure },
        ]
    }
}
