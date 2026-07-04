//! Per-chunk voxel storage.

use crate::coords::{CHUNK_CUBED, voxel_index};

/// A block id. `0` is always Air (see `blocks.yaml` ordering).
pub type Voxel = u8;

/// Air block id.
pub const AIR: Voxel = 0;

/// A dense `32^3` grid of block ids for one chunk.
///
/// Stored heap-allocated so chunks are cheap to move around the ECS.
#[derive(Clone)]
pub struct ChunkVolume {
    data: Box<[Voxel]>,
}

impl ChunkVolume {
    /// An all-Air chunk.
    pub fn empty() -> Self {
        Self { data: vec![AIR; CHUNK_CUBED].into_boxed_slice() }
    }

    /// Build from a raw voxel buffer (must be exactly `CHUNK_CUBED` long).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), CHUNK_CUBED, "voxel buffer must be 32^3");
        Self { data: bytes.to_vec().into_boxed_slice() }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32, z: i32) -> Voxel {
        self.data[voxel_index(x, y, z)]
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, z: i32, value: Voxel) {
        self.data[voxel_index(x, y, z)] = value;
    }

    /// Raw buffer, for compression / network transmission.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Mutable raw buffer (bulk fills in codecs/tests).
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// True if every voxel is Air.
    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|&v| v == AIR)
    }
}

impl Default for ChunkVolume {
    fn default() -> Self {
        Self::empty()
    }
}
