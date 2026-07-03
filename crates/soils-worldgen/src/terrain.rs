//! Procedural terrain generation, ported from `Chunk.generate` in `server.js`.
//!
//! The math mirrors the JS multi-octave simplex heightmap, soil gradient, rock
//! variation, and 3D-noise caves. Because the Rust `noise` crate uses a
//! different simplex implementation and PRNG than the JS `alea`+`simplex-noise`
//! pair, the terrain is equivalent in character but not byte-identical.

use noise::{NoiseFn, Simplex};
use rayon::prelude::*;
use soils_protocol::{CHUNK_SIZE, ChunkVolume, chunk_origin};

use crate::blocks::BlockRegistry;

/// World generation flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldType {
    /// Rolling simplex terrain with rocks and caves.
    Normal,
    /// Flat ground at a fixed height.
    Flat,
}

/// Resolved block ids for the soil gradient, looked up once per generation.
struct Palette {
    air: u8,
    grass: u8,
    slate: u8,
    stone: u8,
    rocky_dirt: u8,
    tough_dirt: u8,
    dirt: u8,
}

impl Palette {
    fn new(reg: &BlockRegistry) -> Self {
        let id = |name: &str| reg.id_of(name).unwrap_or(0);
        Self {
            air: 0,
            grass: id("Grass"),
            slate: id("Slate"),
            stone: id("Stone"),
            rocky_dirt: id("Rocky Dirt"),
            tough_dirt: id("Tough Dirt"),
            dirt: id("Dirt"),
        }
    }
}

/// Stateless terrain generator seeded once and reused for every chunk.
pub struct TerrainGen {
    noise: Simplex,
    world_type: WorldType,
}

impl TerrainGen {
    pub fn new(seed: u32, world_type: WorldType) -> Self {
        Self { noise: Simplex::new(seed), world_type }
    }

    #[inline]
    fn n2(&self, x: f64, z: f64) -> f64 {
        self.noise.get([x, z])
    }

    #[inline]
    fn n3(&self, x: f64, y: f64, z: f64) -> f64 {
        self.noise.get([x, y, z])
    }

    /// Generate many chunks in parallel. `generate` takes only shared borrows
    /// (`&self`, `&reg`), so a fresh world's chunk burst fans out across all
    /// cores instead of running serially. Results are returned in input order.
    pub fn generate_batch(
        &self,
        positions: &[glam::IVec3],
        reg: &BlockRegistry,
    ) -> Vec<ChunkVolume> {
        positions.par_iter().map(|&p| self.generate(p, reg)).collect()
    }

    /// Generate one chunk at the given chunk coordinate.
    pub fn generate(&self, chunk_pos: glam::IVec3, reg: &BlockRegistry) -> ChunkVolume {
        let pal = Palette::new(reg);
        let origin = chunk_origin(chunk_pos);
        let mut vol = ChunkVolume::empty();

        for x in 0..CHUNK_SIZE {
            let gx = (origin.x + x) as f64;
            for z in 0..CHUNK_SIZE {
                let gz = (origin.z + z) as f64;

                let height = match self.world_type {
                    WorldType::Flat => 256,
                    WorldType::Normal => {
                        256 + (self.n2(gx / 1000.0, gz / 1000.0) * 50.0
                            - self.n2(gx / 500.0, gz / 500.0) * 30.0
                            + self.n2(gx / 250.0, gz / 250.0) * 20.0
                            - self.n2(gx / 75.0, gz / 75.0) * 10.0
                            + self.n2(gx / 25.0, gz / 25.0) * 5.0)
                            .floor() as i32
                    }
                };

                let rock = if self.world_type == WorldType::Normal {
                    self.n2(gx / 15.0, gz / 15.0) * 5.0
                        - self.n2(gx / 45.0, gz / 45.0).abs() * 10.0
                        - self.n2(gx / 25.0, gz / 25.0).abs() * 15.0
                } else {
                    0.0
                };

                for y in 0..CHUNK_SIZE {
                    let gy = origin.y + y;

                    // Soil gradient by depth below the surface.
                    let mut val = if gy <= height {
                        if gy == height {
                            pal.grass
                        } else if gy < height - 64 {
                            pal.slate
                        } else if gy < height - 32 {
                            pal.stone
                        } else if gy < height - 16 {
                            pal.rocky_dirt
                        } else if gy < height - 8 {
                            pal.tough_dirt
                        } else {
                            pal.dirt
                        }
                    } else {
                        pal.air
                    };

                    if self.world_type == WorldType::Normal {
                        // Surface rock outcrops.
                        if gy > height - 2 && (gy as f64) <= height as f64 + rock {
                            val = pal.stone;
                        }
                        // Caves carved from solid ground.
                        if val != pal.air {
                            let cave = self.n3(gx / 45.0, gy as f64 / 45.0, gz / 45.0).abs();
                            if cave > 0.7 {
                                val = pal.air;
                            }
                        }
                    }

                    if val != pal.air {
                        vol.set(x, y, z, val);
                    }
                }
            }
        }
        vol
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soils_protocol::AIR;

    fn registry() -> BlockRegistry {
        let yaml = "Air:\n  faces: [0,0,0]\nDirt:\n  faces: [1,1,1]\nGrass:\n  faces: [3,2,1]\nStone:\n  faces: [4,4,4]\nSlate:\n  faces: [13,13,13]\nTough Dirt:\n  faces: [14,14,14]\nRocky Dirt:\n  faces: [15,15,15]\n";
        BlockRegistry::from_yaml(yaml).unwrap()
    }

    #[test]
    fn flat_world_has_grass_on_top_and_air_above() {
        let reg = registry();
        let tg = TerrainGen::new(0, WorldType::Flat);
        // Surface y=256 lives in chunk y=8 (256>>5), local y=0.
        let chunk = tg.generate(glam::IVec3::new(0, 8, 0), &reg);
        let grass = reg.id_of("Grass").unwrap();
        let dirt = reg.id_of("Dirt").unwrap();
        assert_eq!(chunk.get(0, 0, 0), grass, "y=256 should be grass");
        // y=257 (local y=1) should be air.
        assert_eq!(chunk.get(0, 1, 0), AIR, "above surface should be air");
        // A chunk fully below the surface should be solid dirt/stone, no air.
        let below = tg.generate(glam::IVec3::new(0, 7, 0), &reg);
        assert_ne!(below.get(0, 31, 0), AIR);
        let _ = dirt;
    }

    #[test]
    fn generate_batch_matches_sequential() {
        let reg = registry();
        let tg = TerrainGen::new(1234, WorldType::Normal);
        let positions: Vec<glam::IVec3> = (0..6)
            .map(|i| glam::IVec3::new(i % 3, 8 - (i / 3), i))
            .collect();
        let batched = tg.generate_batch(&positions, &reg);
        assert_eq!(batched.len(), positions.len());
        for (pos, got) in positions.iter().zip(&batched) {
            let expected = tg.generate(*pos, &reg);
            assert_eq!(
                got.as_bytes(),
                expected.as_bytes(),
                "batched chunk {pos:?} differs from sequential generate"
            );
        }
    }
}
