//! Pure world-generation and meshing logic shared by the client and server.
//! No Bevy or networking dependencies, so it is fast to test in isolation.

pub mod blocks;
pub mod fx;
pub mod graph;
pub mod greedy;
pub mod noise_det;
pub mod radiance;
pub mod terrain;
pub mod wgsl;

/// Bumped whenever generated output changes for the same (seed, graph, world
/// type) — noise algorithm changes, threshold retunes, table edits. The server
/// stamps worlds with it (chunks that no longer reproduce reclassify as
/// edited) and it feeds the protocol's `graph_hash`.
pub const WORLDGEN_ALGO_VERSION: u32 = 2;

/// Stable identity of a generator: FNV-1a over the RON-serialized graph plus
/// [`WORLDGEN_ALGO_VERSION`]. Client and server compare these to decide
/// whether client-side generation can reproduce server chunks bit-exactly.
pub fn graph_hash(graph: &TerrainGraph) -> u64 {
    let text = ron::to_string(graph).expect("graph serializes");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes().chain(WORLDGEN_ALGO_VERSION.to_le_bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

pub use blocks::{BlockDef, BlockRegistry};
pub use graph::{CaveParams, ColumnSample, CompiledGraph, NodeKind, TerrainGraph};
pub use greedy::{MeshData, greedy_mesh};
pub use radiance::{LightGrid, Radiance};
pub use terrain::{TerrainGen, WorldType};

/// The original block table, embedded so both binaries can build a registry
/// without shipping a data file. Mirrors `server/public/files/blocks.yaml`.
pub const BLOCKS_YAML: &str = include_str!("../blocks.yaml");

/// Convenience constructor for the standard block registry.
pub fn default_registry() -> BlockRegistry {
    BlockRegistry::from_yaml(BLOCKS_YAML).expect("embedded blocks.yaml is valid")
}
