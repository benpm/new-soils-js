//! Light-flood benchmarks. Run with `cargo bench -p soils-sim`.
//!
//! Contrasts the "assume full light" fast path against the general flood:
//! - `full_sky_fast_path`   — `light_new_chunk` on an all-air open-sky chunk
//!                            (uniform sky=15 in one shot, no BFS).
//! - `full_sky_full_flood`  — `relight_full` on the same chunk (the old cost:
//!                            zero + seed scan + whole-grid BFS).
//! - `surface_full_flood`   — `light_new_chunk` on a real surface chunk, which
//!                            bails the fast path and runs the full flood.

use criterion::{Criterion, criterion_group, criterion_main};
use glam::IVec3;
use soils_protocol::{CHUNK_SIZE, chunk_origin};
use soils_sim::light::{self, LightWorld};
use soils_worldgen::{TerrainGen, WorldType, default_registry};
use std::hint::black_box;

/// A dense single-chunk world with open sky above.
#[derive(Clone)]
struct Chunk {
    origin: IVec3,
    solid: Vec<bool>,
    light: Vec<u8>,
    all_air: bool,
}

impl Chunk {
    fn air(cpos: IVec3) -> Self {
        let n = (CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE) as usize;
        Self { origin: chunk_origin(cpos), solid: vec![false; n], light: vec![0; n], all_air: true }
    }

    fn surface(cpos: IVec3) -> Self {
        let mut c = Self::air(cpos);
        let vol = TerrainGen::new(0, WorldType::Normal).generate(cpos, &default_registry());
        for z in 0..CHUNK_SIZE {
            for y in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    if vol.get(x, y, z) != 0 {
                        c.solid[Self::idx(x, y, z)] = true;
                        c.all_air = false;
                    }
                }
            }
        }
        c
    }

    #[inline]
    fn idx(x: i32, y: i32, z: i32) -> usize {
        (x + CHUNK_SIZE * (y + CHUNK_SIZE * z)) as usize
    }
    #[inline]
    fn local(&self, v: IVec3) -> IVec3 {
        v - self.origin
    }
}

impl LightWorld for Chunk {
    fn solid(&self, v: IVec3) -> bool {
        let l = self.local(v);
        self.in_domain(v) && self.solid[Self::idx(l.x, l.y, l.z)]
    }
    fn emission(&self, _v: IVec3) -> u8 {
        0
    }
    fn light(&self, v: IVec3) -> u8 {
        let l = self.local(v);
        if self.in_domain(v) { self.light[Self::idx(l.x, l.y, l.z)] } else { 0 }
    }
    fn set_light(&mut self, v: IVec3, packed: u8) {
        if self.in_domain(v) {
            let l = self.local(v);
            self.light[Self::idx(l.x, l.y, l.z)] = packed;
        }
    }
    fn in_domain(&self, v: IVec3) -> bool {
        let l = self.local(v);
        l.cmpge(IVec3::ZERO).all() && l.cmplt(IVec3::splat(CHUNK_SIZE)).all()
    }
    fn open_sky_above(&self, _v: IVec3) -> bool {
        true
    }
    fn chunk_all_air(&self, _cpos: IVec3) -> bool {
        self.all_air
    }
}

fn bench_flood(c: &mut Criterion) {
    let cpos = IVec3::new(8, 8, 8);
    let mut g = c.benchmark_group("flood");

    g.bench_function("full_sky_fast_path", |b| {
        let mut w = Chunk::air(cpos);
        b.iter(|| light::light_new_chunk(black_box(&mut w), black_box(cpos)))
    });
    g.bench_function("full_sky_full_flood", |b| {
        let mut w = Chunk::air(cpos);
        let chunks = [cpos];
        b.iter(|| light::relight_full(black_box(&mut w), black_box(&chunks)))
    });
    g.bench_function("surface_full_flood", |b| {
        let mut w = Chunk::surface(cpos);
        b.iter(|| light::light_new_chunk(black_box(&mut w), black_box(cpos)))
    });

    g.finish();
}

criterion_group!(benches, bench_flood);
criterion_main!(benches);
