//! Greedy-mesher benchmarks. Run with `cargo bench -p soils-worldgen --bench mesh`.
//!
//! Meshes real seed-0 chunks (the CPU reference path the GPU mesher mirrors).
//! `merged` is the shipping AO-aware greedy path; `unmerged` isolates the
//! per-face cost without quad merging, so the merge win is visible.

use criterion::{Criterion, criterion_group, criterion_main};
use glam::IVec3;
use soils_worldgen::{TerrainGen, WorldType, default_registry, greedy_mesh};
use std::hint::black_box;

fn bench_mesh(c: &mut Criterion) {
    let reg = default_registry();
    let tg = TerrainGen::new(0, WorldType::Normal);
    // Surface band (mixed air/solid, the common case) and a fully solid chunk.
    let surface = tg.generate(IVec3::new(8, 8, 8), &reg);
    let solid = tg.generate(IVec3::new(8, 4, 8), &reg);

    let mut g = c.benchmark_group("mesh");
    g.bench_function("surface_merged", |b| {
        b.iter(|| black_box(greedy_mesh(black_box(&surface), true)))
    });
    g.bench_function("surface_unmerged", |b| {
        b.iter(|| black_box(greedy_mesh(black_box(&surface), false)))
    });
    g.bench_function("solid_merged", |b| {
        b.iter(|| black_box(greedy_mesh(black_box(&solid), true)))
    });
    g.finish();
}

criterion_group!(benches, bench_mesh);
criterion_main!(benches);
