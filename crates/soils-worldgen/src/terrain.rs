//! Procedural terrain generation, ported from `Chunk.generate` in `server.js`.
//!
//! Worldgen v2: all sampling runs on the deterministic Q16.16 core
//! ([`crate::fx`] + [`crate::noise_det`]), so generation is a bit-exact pure
//! function of `(seed, graph, world_type, chunk_pos)` on every CPU **and** on
//! the GPU (the WGSL mirror). That is what lets clients generate pristine
//! chunks locally instead of streaming them. Terrain is character-equivalent
//! to the earlier f64-simplex port (itself character-equivalent to the JS
//! original), not byte-identical — bumping [`crate::WORLDGEN_ALGO_VERSION`]
//! reclassifies previously persisted chunks as edited.

use rayon::prelude::*;
use soils_protocol::{CHUNK_SIZE, ChunkVolume, chunk_origin};

use crate::blocks::BlockRegistry;
use crate::fx::{self, Fx};
use crate::graph::{CompiledGraph, TerrainGraph};

/// World generation flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldType {
    /// Rolling noise terrain with rocks and caves.
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

/// Cave-noise lattice spacing in voxels. The cave field has a ~45-voxel
/// wavelength, so sampling every 4 voxels and trilinearly interpolating is
/// visually indistinguishable from per-voxel evaluation at ~1/45th the
/// 3D-noise cost. The GPU gen kernel uses the same lattice, so interpolation
/// error is identical on both ends (it's part of the deterministic function).
const CAVE_STEP: i32 = 4;
/// Lattice points per axis: samples at 0, 4, ..., 32 inclusive.
const CAVE_N: usize = (CHUNK_SIZE / CAVE_STEP) as usize + 1;

/// Conservative ceiling on the highest solid voxel the height + outcrop math
/// can produce (256 + summed octave amplitudes 115 scaled by the noise
/// envelope 0.75 ≈ 87, + max rock 5, with margin). Chunks whose origin is
/// above this are all air. Assumes the default-flavoured graph. Shared with
/// the GPU gen kernel codegen (`crate::wgsl`).
pub(crate) const MAX_SURFACE: i32 = 256 + 115 + 5 + 24;
/// Max positive contribution of the rock-outcrop term (the other two terms
/// only subtract).
pub(crate) const MAX_ROCK: i32 = 5;

/// Stateless terrain generator seeded once and reused for every chunk. The
/// height/rock/structure math lives in a [`TerrainGraph`] compiled to the
/// fixed-point evaluator; caves stay a fast trilinear-lattice 3D-noise carve.
pub struct TerrainGen {
    seed: u32,
    graph: TerrainGraph,
    compiled: CompiledGraph,
    world_type: WorldType,
}

impl TerrainGen {
    /// A generator using the default graph (the original soils terrain
    /// character: 5-octave heightmap, rock outcrops, caves).
    pub fn new(seed: u32, world_type: WorldType) -> Self {
        Self::from_graph(TerrainGraph::default_soils(), seed, world_type)
    }

    /// A generator driven by a designed graph (e.g. loaded from a
    /// `*.terrain.ron` produced by `soils-terrainlab`). Panics on a graph that
    /// fails [`TerrainGraph::validate`] — game paths validate at load time.
    pub fn from_graph(graph: TerrainGraph, seed: u32, world_type: WorldType) -> Self {
        let compiled = graph.compile().expect("graph validated before from_graph");
        Self { seed, graph, compiled, world_type }
    }

    /// Load a graph from a `*.terrain.ron` file and build a generator from it.
    pub fn load_ron(path: &std::path::Path, seed: u32, world_type: WorldType) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let graph: TerrainGraph = ron::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        graph
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::from_graph(graph, seed, world_type))
    }

    /// The graph this generator evaluates.
    pub fn graph(&self) -> &TerrainGraph {
        &self.graph
    }

    pub fn seed(&self) -> u32 {
        self.seed
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

    /// Sample the signed cave field on a `CAVE_N`³ lattice covering the chunk
    /// (inclusive of the +1 borders so interpolation never leaves the grid).
    fn cave_lattice(&self, origin: glam::IVec3) -> Vec<Fx> {
        let mut lat = vec![0; CAVE_N * CAVE_N * CAVE_N];
        let mut i = 0;
        for iy in 0..CAVE_N {
            let gy = origin.y + iy as i32 * CAVE_STEP;
            for iz in 0..CAVE_N {
                let gz = origin.z + iz as i32 * CAVE_STEP;
                for ix in 0..CAVE_N {
                    let gx = origin.x + ix as i32 * CAVE_STEP;
                    lat[i] = self.compiled.cave_noise(self.seed, gx, gy, gz);
                    i += 1;
                }
            }
        }
        lat
    }

    /// Trilinearly interpolated signed cave noise for a chunk-local voxel.
    /// Fractions are exact Q16.16 multiples of 1/CAVE_STEP.
    #[inline]
    fn cave_at(lat: &[Fx], x: i32, y: i32, z: i32) -> Fx {
        let (xi, yi, zi) =
            ((x / CAVE_STEP) as usize, (y / CAVE_STEP) as usize, (z / CAVE_STEP) as usize);
        let f = |v: i32| (v % CAVE_STEP) * (fx::ONE / CAVE_STEP);
        let (fxq, fyq, fzq) = (f(x), f(y), f(z));
        let at = |ix: usize, iy: usize, iz: usize| lat[(iy * CAVE_N + iz) * CAVE_N + ix];
        let x00 = fx::lerp(at(xi, yi, zi), at(xi + 1, yi, zi), fxq);
        let x10 = fx::lerp(at(xi, yi + 1, zi), at(xi + 1, yi + 1, zi), fxq);
        let x01 = fx::lerp(at(xi, yi, zi + 1), at(xi + 1, yi, zi + 1), fxq);
        let x11 = fx::lerp(at(xi, yi + 1, zi + 1), at(xi + 1, yi + 1, zi + 1), fxq);
        fx::lerp(fx::lerp(x00, x10, fyq), fx::lerp(x01, x11, fyq), fzq)
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

        let caves_on = self.world_type == WorldType::Normal && self.compiled.caves.is_some();
        let lat = caves_on.then(|| self.cave_lattice(origin));
        let cave_thr = self.compiled.caves.map_or(Fx::MAX, |c| c.threshold);

        for x in 0..CHUNK_SIZE {
            let gx = origin.x + x;
            for z in 0..CHUNK_SIZE {
                let gz = origin.z + z;

                // Sample the graph's surface channels once per column.
                let (height, rock_fx) = match self.world_type {
                    WorldType::Flat => (256, 0),
                    WorldType::Normal => {
                        let (h, r, _) = self.compiled.columns_fx(
                            self.seed,
                            gx.wrapping_shl(16),
                            gz.wrapping_shl(16),
                        );
                        (fx::floor(h), r)
                    }
                };

                // Whole column above the surface (and any outcrop): all air.
                if origin.y > height + MAX_ROCK {
                    continue;
                }

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

                    if let Some(lat) = &lat {
                        // Surface rock outcrops: gy <= height + rock, exact in
                        // Q16.16 (gy and height are small integers).
                        if gy > height - 2 && (gy - height).wrapping_shl(16) <= rock_fx {
                            val = pal.stone;
                        }
                        // Caves carved from solid ground.
                        if val != pal.air && fx::abs(Self::cave_at(lat, x, y, z)) > cave_thr {
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
        assert_eq!(chunk.get(0, 0, 0), grass, "y=256 should be grass");
        // y=257 (local y=1) should be air.
        assert_eq!(chunk.get(0, 1, 0), AIR, "above surface should be air");
        // A chunk fully below the surface should be solid dirt/stone, no air.
        let below = tg.generate(glam::IVec3::new(0, 7, 0), &reg);
        assert_ne!(below.get(0, 31, 0), AIR);
    }

    /// Prints cave density for candidate thresholds; run with `--ignored
    /// --nocapture` when retuning `CaveParams::default().threshold`.
    #[test]
    #[ignore]
    fn measure_cave_density() {
        let reg = registry();
        for thr in [0.45f32, 0.50, 0.55, 0.60, 0.65] {
            let mut graph = TerrainGraph::default_soils();
            graph.caves.threshold = thr;
            let tg = TerrainGen::from_graph(graph, 0, WorldType::Normal);
            let mut carved = 0usize;
            for cx in 6..10 {
                for cz in 6..10 {
                    let chunk = tg.generate(glam::IVec3::new(cx, 4, cz), &reg);
                    carved += chunk.as_bytes().iter().filter(|&&b| b == AIR).count();
                }
            }
            let total = 16 * 32 * 32 * 32;
            println!("thr {thr}: {carved}/{total} = {:.3}%", carved as f64 / total as f64 * 100.0);
        }
    }

    #[test]
    fn caves_are_carved_below_the_surface() {
        // Deep chunks are fully inside the soil gradient, so any air in them
        // must come from cave carving. Require a plausible density band over a
        // 16-chunk region (~1-2% of 524k voxels) — pins the default threshold
        // against both regressions (no caves) and runaway carving.
        let reg = registry();
        let tg = TerrainGen::new(0, WorldType::Normal);
        let mut carved = 0usize;
        for cx in 6..10 {
            for cz in 6..10 {
                let chunk = tg.generate(glam::IVec3::new(cx, 4, cz), &reg);
                carved += chunk.as_bytes().iter().filter(|&&b| b == AIR).count();
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

    /// The height envelope stays under MAX_SURFACE (the all-air early-out must
    /// agree with the full math for every seed/column).
    #[test]
    fn height_envelope_under_max_surface() {
        let tg = TerrainGen::new(0xdead, WorldType::Normal);
        let mut max_h = i32::MIN;
        for i in 0..20_000i32 {
            let x = (crate::noise_det::pcg(i as u32) % 60000) as i32 - 30000;
            let z = (crate::noise_det::pcg(i as u32 ^ 0xabc) % 60000) as i32 - 30000;
            let (h, r, _) =
                tg.compiled.columns_fx(tg.seed, x.wrapping_shl(16), z.wrapping_shl(16));
            max_h = max_h.max(fx::floor(h) + fx::floor(r).max(0) + 1);
        }
        assert!(
            max_h < MAX_SURFACE - 8,
            "height envelope too tight: max {max_h} vs ceiling {MAX_SURFACE}"
        );
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

    /// Golden chunk hashes: pins generation output across platforms and
    /// releases. A deliberate algorithm change re-pins these AND bumps
    /// WORLDGEN_ALGO_VERSION.
    #[test]
    fn golden_chunk_hashes() {
        let reg = registry();
        let tg = TerrainGen::new(0, WorldType::Normal);
        let hash = |v: &ChunkVolume| {
            // FNV-1a over the raw voxel bytes.
            let mut h: u64 = 0xcbf29ce484222325;
            for &b in v.as_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
            h
        };
        let got: Vec<u64> = [
            glam::IVec3::new(0, 8, 0),
            glam::IVec3::new(2, 8, -3),
            glam::IVec3::new(6, 4, 7),
            glam::IVec3::new(-5, 7, 12),
        ]
        .iter()
        .map(|&p| hash(&tg.generate(p, &reg)))
        .collect();
        assert_eq!(got, GOLDEN_HASHES, "generated chunks drifted: {got:?}");
    }

    // Captured from the first correct run with the final tuning constants.
    const GOLDEN_HASHES: [u64; 4] = [
        4649143715739076006,
        10333885101303931685,
        8965844200867111717,
        322037544446518887,
    ];
}
