//! Per-chunk voxel storage.

use crate::coords::{CHUNK_CUBED, voxel_index};

/// A block id. `0` is always Air (see `blocks.yaml` ordering).
pub type Voxel = u8;

/// Air block id.
pub const AIR: Voxel = 0;
/// Number of `u32` words needed for one bit per voxel.
pub const OCCUPANCY_WORDS: usize = CHUNK_CUBED / 32;

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
        Self {
            data: vec![AIR; CHUNK_CUBED].into_boxed_slice(),
        }
    }

    /// Build from a raw voxel buffer (must be exactly `CHUNK_CUBED` long).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), CHUNK_CUBED, "voxel buffer must be 32^3");
        Self {
            data: bytes.to_vec().into_boxed_slice(),
        }
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

    /// Build a one-bit-per-voxel occupancy sidecar in the same
    /// `(y + z * 32) * 32 + x` order as the dense volume.
    ///
    /// This is deliberately derived rather than stored: keeping a 4 KiB
    /// sidecar beside every 32 KiB chunk would increase resident memory for
    /// the common full-detail window. GPU pools can upload this compact form
    /// when a future occupancy-first mesher justifies the extra buffer.
    pub fn occupancy_words(&self) -> Box<[u32; OCCUPANCY_WORDS]> {
        let mut words = Box::new([0; OCCUPANCY_WORDS]);
        for (i, &voxel) in self.data.iter().enumerate() {
            if voxel != AIR {
                words[i >> 5] |= 1u32 << (i & 31);
            }
        }
        words
    }

    /// Count non-air voxels without allocating an occupancy sidecar.
    pub fn occupied_count(&self) -> usize {
        self.data.iter().filter(|&&voxel| voxel != AIR).count()
    }
}

impl Default for ChunkVolume {
    fn default() -> Self {
        Self::empty()
    }
}
