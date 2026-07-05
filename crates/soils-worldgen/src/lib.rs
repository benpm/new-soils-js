//! Pure world-generation and meshing logic shared by the client and server.
//! No Bevy or networking dependencies, so it is fast to test in isolation.

pub mod blocks;
pub mod graph;
pub mod greedy;
pub mod radiance;
pub mod terrain;

pub use blocks::{BlockDef, BlockRegistry};
pub use graph::{CaveParams, ColumnSample, NodeKind, TerrainGraph};
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
