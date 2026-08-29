//! State attached to individual blocks — what a `u8` voxel id cannot carry.
//!
//! A voxel is one byte, so "this is a chest" is all the world array can say.
//! What is *in* the chest lives here, in a side table keyed by position and
//! stored a chunk at a time.
//!
//! Keying by position rather than by an allocated id is the whole trick. There
//! is no id to hand out, no free list to maintain, and no way for the side
//! table to disagree with the world about which block it describes: break the
//! block, drop the entry. It also means a chunk's data is addressed exactly as
//! its voxels are, so both sit at the same slot index in parallel files (see
//! `soils-server`'s `paged`/`store`).
//!
//! Chunk-at-a-time is the unit because that is the unit of everything else:
//! residency, streaming, eviction. A chest cannot be loaded while its chunk is
//! not, and does not need to be.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use soils_protocol::{CHUNK_CLIP, ItemStack, voxel_index};

use crate::Inventory;

/// Position of a block within its chunk. 32^3 fits a `u16` with room to spare.
pub type LocalPos = u16;

/// Local key for an absolute voxel position.
pub fn local_key(x: i32, y: i32, z: i32) -> LocalPos {
    voxel_index(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP) as LocalPos
}

/// Everything one block might remember.
///
/// An enum rather than a component soup: block data is persisted, so every
/// variant is a format commitment, and keeping them enumerable is what makes
/// "can this build read this world?" a question with an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BlockData {
    /// A chest, barrel, crate — anything that holds items. The slot count comes
    /// from the block's definition and is fixed at creation.
    Container(Inventory),
}

impl BlockData {
    /// Nothing worth persisting. An empty chest is exactly as good as no
    /// record of a chest: placing one creates no data, and emptying one throws
    /// its record away rather than storing a row of `None`s forever.
    pub fn is_empty(&self) -> bool {
        match self {
            BlockData::Container(inv) => inv.is_empty(),
        }
    }

    /// Everything this block would spill if it were broken.
    pub fn contents(&self) -> Vec<ItemStack> {
        match self {
            BlockData::Container(inv) => inv.slots().iter().flatten().copied().collect(),
        }
    }
}

/// Every block in one chunk that carries data.
///
/// `BTreeMap` rather than `HashMap` so encoding is deterministic: the same
/// contents produce the same bytes, which is what lets a test compare a
/// round-trip and a writer skip an identical rewrite.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkData {
    blocks: BTreeMap<LocalPos, BlockData>,
}

impl ChunkData {
    pub fn get(&self, key: LocalPos) -> Option<&BlockData> {
        self.blocks.get(&key)
    }

    /// The container at `key`, created empty with `slots` slots if there is
    /// none. A block whose data is a different variant is *replaced* — the
    /// world is authoritative about what block stands there, and a stale entry
    /// of the wrong shape is corruption, not a value to preserve.
    pub fn container_mut(&mut self, key: LocalPos, slots: usize) -> &mut Inventory {
        let entry = self
            .blocks
            .entry(key)
            .or_insert_with(|| BlockData::Container(Inventory::new(slots)));
        if !matches!(entry, BlockData::Container(_)) {
            *entry = BlockData::Container(Inventory::new(slots));
        }
        match entry {
            BlockData::Container(inv) => inv,
        }
    }

    /// Drop `key`'s data and hand it back — what a block break does.
    pub fn remove(&mut self, key: LocalPos) -> Option<BlockData> {
        self.blocks.remove(&key)
    }

    /// Drop entries that hold nothing. Called after every mutation so an
    /// emptied chest stops costing a page.
    pub fn prune(&mut self) {
        self.blocks.retain(|_, d| !d.is_empty());
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.values().all(|d| d.is_empty())
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (LocalPos, &BlockData)> {
        self.blocks.iter().map(|(&k, v)| (k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soils_protocol::ItemKind;

    fn stack(id: u8, n: u16) -> ItemStack {
        ItemStack::new(ItemKind::Block(id), n).unwrap()
    }

    #[test]
    fn two_blocks_in_one_chunk_get_distinct_keys() {
        assert_ne!(local_key(1, 2, 3), local_key(1, 2, 4));
        // Absolute positions in different chunks share a local key — that is
        // the point: the chunk position picks the page, the key picks the row.
        assert_eq!(local_key(1, 2, 3), local_key(33, 2, 3));
        // Negative coordinates wrap into the chunk, not out of range.
        assert!(local_key(-1, -1, -1) < 32768);
    }

    #[test]
    fn a_container_is_created_on_demand_and_pruned_when_emptied() {
        let mut data = ChunkData::default();
        let key = local_key(4, 5, 6);
        assert!(data.get(key).is_none());

        assert!(data.container_mut(key, 27).insert(stack(3, 10)).is_none());
        assert_eq!(data.len(), 1);
        assert!(!data.is_empty());

        // Emptying it leaves an entry until pruned, and nothing after.
        assert_eq!(data.container_mut(key, 27).remove(ItemKind::Block(3), 10), 10);
        assert!(data.is_empty(), "an empty container is not worth persisting");
        data.prune();
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn breaking_a_block_hands_back_everything_it_held() {
        let mut data = ChunkData::default();
        let key = local_key(0, 0, 0);
        let inv = data.container_mut(key, 27);
        assert!(inv.insert(stack(3, 10)).is_none());
        assert!(inv.insert(stack(5, 7)).is_none());

        let spill = data.remove(key).expect("data present").contents();
        assert_eq!(spill.len(), 2);
        assert_eq!(spill.iter().map(|s| s.count as u32).sum::<u32>(), 17);
        assert!(data.get(key).is_none(), "the entry goes with the block");
    }

    #[test]
    fn identical_contents_encode_identically() {
        let build = || {
            let mut d = ChunkData::default();
            for (x, id) in [(1, 3u8), (9, 5), (4, 7)] {
                let _ = d.container_mut(local_key(x, 0, 0), 27).insert(stack(id, 2));
            }
            d
        };
        let a = soils_protocol::encode(&build());
        let b = soils_protocol::encode(&build());
        assert_eq!(a, b, "insertion order must not change the bytes");
        let back: ChunkData = soils_protocol::decode(&a).expect("round trip");
        assert_eq!(back, build());
    }
}
