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

/// Cave-noise lattice spacing in voxels. The cave field is wavelength-45
/// simplex, so sampling every 4 voxels (~11 samples per period) and
/// trilinearly interpolating is visually indistinguishable from per-voxel
/// evaluation at ~1/45th the 3D-noise cost.
const CAVE_STEP: i32 = 4;
/// Lattice points per axis: samples at 0, 4, ..., 32 inclusive.
const CAVE_N: usize = (CHUNK_SIZE / CAVE_STEP) as usize + 1;

/// Conservative ceiling on the highest solid voxel the height + outcrop math
/// can produce (256 + summed octave amplitudes 115 + max rock 5, with margin
/// for simplex overshoot). Chunks whose origin is above this are all air.
const MAX_SURFACE: i32 = 256 + 115 + 5 + 24;
/// Max positive contribution of the rock-outcrop term (the other two terms
/// only subtract).
const MAX_ROCK: i32 = 5;

/// |cave noise| above this carves air. The JS original used 0.7 on a simplex
/// spanning roughly [-1, 1]; the `noise` crate's simplex tops out near 0.75
/// (measured max 0.745 over 500k samples), so 0.7 carved almost nothing —
/// the port had effectively lost its caves. 0.55 restores the original's
/// ~1-2% underground cave density.
const CAVE_THRESHOLD: f64 = 0.55;

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

    /// Generate many chunks in parallel. Generation takes only shared borrows
    /// (`&self`, `&reg`), so a fresh world's chunk burst fans out across all
    /// cores instead of running serially. Results are returned in input order.
    pub fn generate_batch(
        &self,
        positions: &[glam::IVec3],
        reg: &BlockRegistry,
    ) -> Vec<ChunkVolume> {
        let pal = Palette::new(reg);
        positions.par_iter().map(|&p| self.generate_with(&pal, p)).collect()
    }

    /// Generate one chunk at the given chunk coordinate.
    pub fn generate(&self, chunk_pos: glam::IVec3, reg: &BlockRegistry) -> ChunkVolume {
        self.generate_with(&Palette::new(reg), chunk_pos)
    }

    /// Sample the signed cave field on a `CAVE_N`^3 lattice covering the chunk
    /// (inclusive of the +1 borders so interpolation never leaves the grid).
    fn cave_lattice(&self, origin: glam::IVec3) -> Vec<f64> {
        let mut lat = vec![0.0f64; CAVE_N * CAVE_N * CAVE_N];
        let mut i = 0;
        for iy in 0..CAVE_N {
            let gy = (origin.y + iy as i32 * CAVE_STEP) as f64;
            for iz in 0..CAVE_N {
                let gz = (origin.z + iz as i32 * CAVE_STEP) as f64;
                for ix in 0..CAVE_N {
                    let gx = (origin.x + ix as i32 * CAVE_STEP) as f64;
                    lat[i] = self.n3(gx / 45.0, gy / 45.0, gz / 45.0);
                    i += 1;
                }
            }
        }
        lat
    }

    /// Trilinearly interpolated signed cave noise for a chunk-local voxel.
    #[inline]
    fn cave_at(lat: &[f64], x: i32, y: i32, z: i32) -> f64 {
        let (xi, yi, zi) = ((x / CAVE_STEP) as usize, (y / CAVE_STEP) as usize, (z / CAVE_STEP) as usize);
        let f = |v: i32| (v % CAVE_STEP) as f64 / CAVE_STEP as f64;
        let (fx, fy, fz) = (f(x), f(y), f(z));
        let at = |ix: usize, iy: usize, iz: usize| lat[(iy * CAVE_N + iz) * CAVE_N + ix];
        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        let x00 = lerp(at(xi, yi, zi), at(xi + 1, yi, zi), fx);
        let x10 = lerp(at(xi, yi + 1, zi), at(xi + 1, yi + 1, zi), fx);
        let x01 = lerp(at(xi, yi, zi + 1), at(xi + 1, yi, zi + 1), fx);
        let x11 = lerp(at(xi, yi + 1, zi + 1), at(xi + 1, yi + 1, zi + 1), fx);
        lerp(lerp(x00, x10, fy), lerp(x01, x11, fy), fz)
    }

    fn generate_with(&self, pal: &Palette, chunk_pos: glam::IVec3) -> ChunkVolume {
        let origin = chunk_origin(chunk_pos);
        let mut vol = ChunkVolume::empty();

        // Nothing can be solid this high up, whatever the noise does.
        let ceiling = match self.world_type {
            WorldType::Flat => 256,
            WorldType::Normal => MAX_SURFACE,
        };
        if origin.y > ceiling {
            return vol;
        }

        let cave = match self.world_type {
            WorldType::Normal => Some(self.cave_lattice(origin)),
            WorldType::Flat => None,
        };

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

                // Whole column above the surface (and any outcrop): all air.
                if origin.y > height + MAX_ROCK {
                    continue;
                }

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

                    if let Some(lat) = &cave {
                        // Surface rock outcrops.
                        if gy > height - 2 && (gy as f64) <= height as f64 + rock {
                            val = pal.stone;
                        }
                        // Caves carved from solid ground.
                        if val != pal.air && Self::cave_at(lat, x, y, z).abs() > CAVE_THRESHOLD {
                            val = pal.air;
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
    fn caves_are_carved_below_the_surface() {
        // Deep chunks are fully inside the soil gradient, so any air in them
        // must come from cave carving. Require a plausible density band over a
        // 16-chunk region (~1-2% of 524k voxels) — pins CAVE_THRESHOLD against
        // both regressions (no caves) and runaway carving (swiss cheese).
        let reg = registry();
        let tg = TerrainGen::new(0, WorldType::Normal);
        let mut carved = 0usize;
        for cx in 6..10 {
            for cz in 6..10 {
                let chunk = tg.generate(glam::IVec3::new(cx, 4, cz), &reg);
                for x in 0..CHUNK_SIZE {
                    for y in 0..CHUNK_SIZE {
                        for z in 0..CHUNK_SIZE {
                            if chunk.get(x, y, z) == AIR {
                                carved += 1;
                            }
                        }
                    }
                }
            }
        }
        let total = 16 * 32 * 32 * 32;
        assert!(
            carved > total / 200 && carved < total / 10,
            "cave density off: {carved}/{total} air voxels"
        );
    }

    #[test]
    fn sky_chunks_are_empty() {
        let reg = registry();
        let tg = TerrainGen::new(0, WorldType::Normal);
        // Above MAX_SURFACE: the early-out must agree with the full math.
        assert!(tg.generate(glam::IVec3::new(8, 14, 8), &reg).is_empty());
        assert!(tg.generate(glam::IVec3::new(-3, 20, 5), &reg).is_empty());
        let flat = TerrainGen::new(0, WorldType::Flat);
        assert!(flat.generate(glam::IVec3::new(0, 9, 0), &reg).is_empty());
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
