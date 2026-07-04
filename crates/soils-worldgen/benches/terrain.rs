//! Worldgen benchmarks (TODO phase 4). Run with `cargo bench -p soils-worldgen`.
//!
//! `wave48` models one server generation wave (`soils-server` WAVE_SIZE = 48)
//! from a fresh world's spawn burst; the single-chunk benches isolate the cost
//! classes: a surface chunk (mixed air/solid), a fully solid chunk (worst-case
//! cave-noise load), and a fully air chunk (should be near-free).

use criterion::{Criterion, criterion_group, criterion_main};
use glam::IVec3;
use soils_worldgen::{TerrainGen, WorldType, default_registry};
use std::hint::black_box;

/// The first 48 positions of a radius-4 interest cube around the spawn chunk
/// (8,8,8) in the same nested order the client requests them.
fn wave_positions() -> Vec<IVec3> {
    let mut out = Vec::new();
    for x in 4..=12 {
        for y in 4..=12 {
            for z in 4..=12 {
                out.push(IVec3::new(x, y, z));
                if out.len() == 48 {
                    return out;
                }
            }
        }
    }
    unreachable!()
}

fn bench_terrain(c: &mut Criterion) {
    let reg = default_registry();
    let tg = TerrainGen::new(0, WorldType::Normal);
    let wave = wave_positions();

    let mut g = c.benchmark_group("terrain");
    g.sample_size(20);
    g.bench_function("wave48", |b| {
        b.iter(|| black_box(tg.generate_batch(black_box(&wave), &reg)))
    });
    // Surface band: spawn chunk itself.
    g.bench_function("surface_chunk", |b| {
        b.iter(|| black_box(tg.generate(black_box(IVec3::new(8, 8, 8)), &reg)))
    });
    // Deep underground: every voxel solid, so every voxel pays cave noise.
    g.bench_function("solid_chunk", |b| {
        b.iter(|| black_box(tg.generate(black_box(IVec3::new(8, 4, 8)), &reg)))
    });
    // High in the sky: all air.
    g.bench_function("air_chunk", |b| {
        b.iter(|| black_box(tg.generate(black_box(IVec3::new(8, 14, 8)), &reg)))
    });
    g.finish();
}

criterion_group!(benches, bench_terrain);
criterion_main!(benches);
