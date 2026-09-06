//! Chunk wire-codec benchmarks (palette + bit-pack + LZ4).
//! Run with `cargo bench -p soils-protocol`.
//!
//! Covers the three payload classes the wire produces: a layered surface chunk
//! (the common streamed case), a uniform-solid chunk, and all-air (the 2-byte
//! degenerate case). Encode and decode are the client's per-streamed-chunk cost.

use criterion::{Criterion, criterion_group, criterion_main};
use soils_protocol::{ChunkVolume, decode_chunk, encode_chunk};
use std::hint::black_box;

/// Air over soil over stone with one ore voxel — the shape of a real surface
/// chunk (mirrors the `terrain_like_chunk_compresses_hard` codec test).
fn surface_chunk() -> ChunkVolume {
    let mut v = ChunkVolume::empty();
    for y in 0..32 {
        for x in 0..32 {
            for z in 0..32 {
                let id = match y {
                    0..12 => 3,
                    12..16 => 2,
                    16 => 1,
                    _ => 0,
                };
                v.set(x, y, z, id);
            }
        }
    }
    v.set(5, 5, 5, 7);
    v
}

fn solid_chunk() -> ChunkVolume {
    let mut v = ChunkVolume::empty();
    v.as_bytes_mut().fill(3);
    v
}

fn sparse_chunk() -> ChunkVolume {
    let mut v = ChunkVolume::empty();
    for i in 0..256 {
        v.as_bytes_mut()[i * 97 % soils_protocol::CHUNK_CUBED] = (i % 7 + 1) as u8;
    }
    v
}

fn bench_codec(c: &mut Criterion) {
    let air = ChunkVolume::empty();
    let surface = surface_chunk();
    let solid = solid_chunk();
    let sparse = sparse_chunk();
    let enc_surface = encode_chunk(&surface);
    let enc_sparse = encode_chunk(&sparse);

    let mut g = c.benchmark_group("codec");
    g.bench_function("encode_surface", |b| {
        b.iter(|| black_box(encode_chunk(black_box(&surface))))
    });
    g.bench_function("encode_solid", |b| {
        b.iter(|| black_box(encode_chunk(black_box(&solid))))
    });
    g.bench_function("encode_air", |b| {
        b.iter(|| black_box(encode_chunk(black_box(&air))))
    });
    g.bench_function("decode_surface", |b| {
        b.iter(|| black_box(decode_chunk(black_box(&enc_surface))))
    });
    g.bench_function("encode_sparse_occupancy", |b| {
        b.iter(|| black_box(encode_chunk(black_box(&sparse))))
    });
    g.bench_function("decode_sparse_occupancy", |b| {
        b.iter(|| black_box(decode_chunk(black_box(&enc_sparse))))
    });
    g.bench_function("build_occupancy_32k", |b| {
        b.iter(|| black_box(black_box(&sparse).occupancy_words()))
    });
    println!(
        "codec sizes: air={} solid={} sparse={} surface={} bytes",
        encode_chunk(&air).len(),
        encode_chunk(&solid).len(),
        enc_sparse.len(),
        enc_surface.len()
    );
    g.finish();
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
