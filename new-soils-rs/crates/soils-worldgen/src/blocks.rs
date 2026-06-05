//! Block definitions, parsed from the original `blocks.yaml`.
//!
//! Block ids are assigned by insertion order in the YAML file, exactly as the
//! JS `Block.parseYaml` did (Air=0, Dirt=1, Grass=2, ...). The `faces` array is
//! `[sides, top, bottom]` of atlas tile indices.

use indexmap::IndexMap;
use serde::Deserialize;

/// A single block type.
#[derive(Debug, Clone)]
pub struct BlockDef {
    pub name: String,
    /// Atlas tile indices: `[sides, top, bottom]`.
    pub faces: [u8; 3],
}

impl BlockDef {
    /// Tile index for a face given its unit normal, matching the JS formula in
    /// `voxels.js`: `faces[((nx+1)*3 + (ny+1)*2 + (nz+1)) % 6 - 1]`.
    pub fn tile_for_normal(&self, n: [i32; 3]) -> u8 {
        // sides/top/bottom expanded to the 6 face directions the JS code used.
        // faces layout in JS Block: [sides, top, sides, bottom, sides] indexed
        // by the normal hash below. We collapse to [sides, top, bottom].
        let idx = (((n[0] + 1) * 3 + (n[1] + 1) * 2 + (n[2] + 1)) as usize % 6).wrapping_sub(1);
        // Map the 5-entry JS face table [sides, top, sides, bottom, sides].
        let table = [self.faces[0], self.faces[1], self.faces[0], self.faces[2], self.faces[0]];
        table[idx.min(table.len() - 1)]
    }
}

/// Ordered registry of all block types; index == block id.
#[derive(Debug, Clone, Default)]
pub struct BlockRegistry {
    blocks: Vec<BlockDef>,
    by_name: std::collections::HashMap<String, u8>,
}

#[derive(Deserialize)]
struct YamlBlock {
    faces: Vec<u8>,
}

impl BlockRegistry {
    /// Parse the original `blocks.yaml` content. Preserves declaration order.
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        let parsed: IndexMap<String, YamlBlock> =
            serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
        let mut reg = BlockRegistry::default();
        for (name, b) in parsed {
            let faces = match b.faces.len() {
                1 => [b.faces[0], b.faces[0], b.faces[0]],
                2 => [b.faces[0], b.faces[1], b.faces[0]],
                _ => [b.faces[0], b.faces[1], b.faces[2]],
            };
            reg.push(BlockDef { name, faces });
        }
        Ok(reg)
    }

    fn push(&mut self, def: BlockDef) {
        let id = self.blocks.len() as u8;
        self.by_name.insert(def.name.clone(), id);
        self.blocks.push(def);
    }

    pub fn get(&self, id: u8) -> Option<&BlockDef> {
        self.blocks.get(id as usize)
    }

    pub fn id_of(&self, name: &str) -> Option<u8> {
        self.by_name.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = "Air:\n  faces: [0, 0, 0]\nDirt:\n  faces: [1, 1, 1]\nGrass:\n  faces: [3, 2, 1]\n";

    #[test]
    fn parses_in_order() {
        let reg = BlockRegistry::from_yaml(YAML).unwrap();
        assert_eq!(reg.id_of("Air"), Some(0));
        assert_eq!(reg.id_of("Dirt"), Some(1));
        assert_eq!(reg.id_of("Grass"), Some(2));
        assert_eq!(reg.get(2).unwrap().faces, [3, 2, 1]);
    }

    #[test]
    fn grass_top_uses_top_tile() {
        let reg = BlockRegistry::from_yaml(YAML).unwrap();
        let grass = reg.get(2).unwrap();
        // Upward normal should select the "top" tile (2).
        assert_eq!(grass.tile_for_normal([0, 1, 0]), 2);
    }
}
