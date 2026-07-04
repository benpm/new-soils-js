//! Headless GPU validation of the compute greedy mesher.
//!
//! Runs `assets/shaders/voxel_mesh.wgsl` (clear_counter + mesh_slice) on a real
//! wgpu device over controlled voxel scenes, reads the quad buffer back, and
//! asserts it matches the CPU oracle `soils_worldgen::greedy_mesh` as a
//! multiset (GPU emit order is nondeterministic across workgroups). Also pins
//! the overflow contract: the atomic count keeps incrementing past MAX_QUADS
//! while writes are dropped, so readers must clamp. Skips gracefully if no GPU
//! adapter is available.

use std::collections::HashMap;

use soils_protocol::{CHUNK_SIZE, ChunkVolume};
use soils_worldgen::greedy_mesh;
use wgpu::util::DeviceExt;

// Must match voxel_mesh.wgsl / gpu_mesh.rs.
const MAX_QUADS: u32 = 8192;
const QUAD_BYTES: usize = 80;
const QUAD_BUFFER_BYTES: u64 = 16 + MAX_QUADS as u64 * QUAD_BYTES as u64;

/// Canonical quad for exact comparison. AO is stored as the integer occlusion
/// level 0..3, recovered from the brightness `0.1 + level * 0.3` both sides
/// compute identically.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct Quad {
    base: [i32; 3],
    du: [i32; 3],
    dv: [i32; 3],
    norm: [i32; 3],
    id: u32,
    ao: [u8; 4],
}

fn ao_level(bright: f32) -> u8 {
    let level = ((bright - 0.1) / 0.3).round();
    assert!(
        (0.0..=3.0).contains(&level) && (bright - (0.1 + level * 0.3)).abs() < 1e-4,
        "ao brightness {bright} is not 0.1 + 0.3k"
    );
    level as u8
}

fn iv(v: [f32; 3]) -> [i32; 3] {
    let r = [v[0].round() as i32, v[1].round() as i32, v[2].round() as i32];
    assert!(
        (0..3).all(|i| (v[i] - r[i] as f32).abs() < 1e-4),
        "non-integer quad component {v:?}"
    );
    r
}

/// CPU oracle quads in canonical form. With the identity faces table used by
/// the GPU side (`faces[id] = (id, id, id)`), tile == block id for every
/// normal, so `id` compares directly against the GPU tile.
fn cpu_quads(vol: &ChunkVolume) -> Vec<Quad> {
    let mesh = greedy_mesh(vol, true);
    let n = mesh.block_ids.len();
    (0..n)
        .map(|i| {
            let p = |k: usize| mesh.positions[4 * i + k];
            let base = p(0);
            let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
            Quad {
                base: iv(base),
                du: iv(sub(p(1), base)),
                dv: iv(sub(p(3), base)),
                norm: iv(mesh.normals[4 * i]),
                id: mesh.block_ids[i] as u32,
                ao: [
                    ao_level(mesh.ao[4 * i]),
                    ao_level(mesh.ao[4 * i + 1]),
                    ao_level(mesh.ao[4 * i + 2]),
                    ao_level(mesh.ao[4 * i + 3]),
                ],
            }
        })
        .collect()
}

/// Dispatch the full GPU mesher (clear + mesh + finalize) over `vol` and read
/// back (raw quad count before finalize's clamp, stored quads, indirect args).
fn gpu_mesh_chunk(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    vol: &ChunkVolume,
) -> (u32, Vec<Quad>, [u32; 4]) {
    let voxels = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("voxels"),
        contents: vol.as_bytes(),
        usage: wgpu::BufferUsages::STORAGE,
    });
    // Identity faces table: tile == block id whatever the face direction.
    let faces: Vec<u32> = (0..256u32).flat_map(|id| [id, id, id, 0]).collect();
    let faces_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("faces"),
        contents: bytemuck::cast_slice(&faces),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let quads = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quads"),
        size: QUAD_BUFFER_BYTES,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let indirect = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("indirect"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    // Quad buffer + raw pre-finalize count + post-finalize indirect args.
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: QUAD_BUFFER_BYTES + 4 + 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/shaders/voxel_mesh.wgsl"
    ))
    .expect("read voxel_mesh.wgsl");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("voxel_mesh"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });

    let buf_entry = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("mesher_layout"),
        entries: &[buf_entry(0, true), buf_entry(1, false), buf_entry(2, true), buf_entry(3, false)],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("mesher_pl"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = |entry: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        })
    };
    let clear = pipeline("clear_counter");
    let mesh = pipeline("mesh_slice");
    let finalize = pipeline("finalize_mesh");

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("mesher_bg"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: voxels.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: quads.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: faces_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: indirect.as_entire_binding() },
        ],
    });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder
            .begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_pipeline(&clear);
        pass.dispatch_workgroups(1, 1, 1);
        pass.set_pipeline(&mesh);
        pass.dispatch_workgroups(3, 33, 1);
    }
    // Snapshot the raw overflow-capable count before finalize clamps it.
    encoder.copy_buffer_to_buffer(&quads, 0, &readback, QUAD_BUFFER_BYTES, 4);
    {
        let mut pass = encoder
            .begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_pipeline(&finalize);
        pass.dispatch_workgroups(1, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&quads, 0, &readback, 0, QUAD_BUFFER_BYTES);
    encoder.copy_buffer_to_buffer(&indirect, 0, &readback, QUAD_BUFFER_BYTES + 4, 16);
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();

    let raw_count = u32::from_le_bytes(
        data[QUAD_BUFFER_BYTES as usize..QUAD_BUFFER_BYTES as usize + 4].try_into().unwrap(),
    );
    let clamped_count = u32::from_le_bytes(data[0..4].try_into().unwrap());
    assert_eq!(clamped_count, raw_count.min(MAX_QUADS), "finalize clamps the stored count");
    let args_off = QUAD_BUFFER_BYTES as usize + 4;
    let args: [u32; 4] = std::array::from_fn(|i| {
        u32::from_le_bytes(data[args_off + 4 * i..args_off + 4 * i + 4].try_into().unwrap())
    });
    let stored = raw_count.min(MAX_QUADS) as usize;
    let mut out = Vec::with_capacity(stored);
    for qi in 0..stored {
        let b = &data[16 + qi * QUAD_BYTES..16 + (qi + 1) * QUAD_BYTES];
        let f = |o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        out.push(Quad {
            base: iv([f(0), f(4), f(8)]),
            id: u(12),
            du: iv([f(16), f(20), f(24)]),
            dv: iv([f(32), f(36), f(40)]),
            norm: iv([f(28), f(44), f(64)]),
            ao: [ao_level(f(48)), ao_level(f(52)), ao_level(f(56)), ao_level(f(60))],
        });
    }
    drop(data);
    readback.unmap();
    (raw_count, out, args)
}

fn assert_scene_matches(device: &wgpu::Device, queue: &wgpu::Queue, name: &str, vol: &ChunkVolume) {
    let mut cpu = cpu_quads(vol);
    assert!(
        cpu.len() < MAX_QUADS as usize,
        "{name}: scene overflows ({len} quads); use the overflow test for that",
        len = cpu.len()
    );
    let (raw_count, mut gpu, args) = gpu_mesh_chunk(device, queue, vol);
    assert_eq!(raw_count as usize, cpu.len(), "{name}: quad count mismatch");
    assert_eq!(args, [cpu.len() as u32 * 6, 1, 0, 0], "{name}: indirect draw args");
    cpu.sort_unstable();
    gpu.sort_unstable();
    assert_eq!(cpu, gpu, "{name}: quad multisets differ");
}

#[test]
fn gpu_mesher_matches_cpu_oracle() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter available; skipping gpu_mesher_matches_cpu_oracle");
        return;
    };

    let mut single = ChunkVolume::empty();
    single.set(5, 5, 5, 3);
    assert_scene_matches(&device, &queue, "single voxel", &single);

    let mut slab = ChunkVolume::empty();
    for x in 0..4 {
        for z in 0..4 {
            slab.set(x, 0, z, 1);
        }
    }
    assert_scene_matches(&device, &queue, "slab", &slab);

    // Terrain-ish heightmap with layered ids: exercises heavy merging, id
    // boundaries, and chunk-border faces.
    let mut terrain = ChunkVolume::empty();
    for x in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            let h = 4 + ((x / 4) * 3 + (z / 4) * 5) % 9;
            for y in 0..h {
                terrain.set(x, y, z, (1 + y % 3) as u8);
            }
        }
    }
    assert_scene_matches(&device, &queue, "terrain", &terrain);

    // Sparse deterministic scatter (LCG): isolated voxels with varied AO
    // interactions, ids 1..=7.
    let mut scatter = ChunkVolume::empty();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for x in 0..CHUNK_SIZE {
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if (state >> 33) % 100 < 3 {
                    scatter.set(x, y, z, (1 + (state >> 40) % 7) as u8);
                }
            }
        }
    }
    assert_scene_matches(&device, &queue, "scatter", &scatter);
}

/// A 3D checkerboard produces 16384 isolated voxels * 6 faces = 98304 quads,
/// far past MAX_QUADS. The contract: the atomic count reports the true total
/// (readers clamp), exactly MAX_QUADS quads are stored, and every stored quad
/// is a real quad from the CPU oracle's multiset.
#[test]
fn gpu_mesher_overflow_is_clamped_and_valid() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter available; skipping gpu_mesher_overflow_is_clamped_and_valid");
        return;
    };

    let mut vol = ChunkVolume::empty();
    for x in 0..CHUNK_SIZE {
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                if (x + y + z) % 2 == 0 {
                    vol.set(x, y, z, 1);
                }
            }
        }
    }

    let cpu = cpu_quads(&vol);
    assert_eq!(cpu.len(), 98304, "checkerboard face count");
    let (raw_count, gpu, args) = gpu_mesh_chunk(&device, &queue, &vol);
    assert_eq!(raw_count as usize, cpu.len(), "raw atomic count reports the true total");
    assert_eq!(gpu.len(), MAX_QUADS as usize, "stored quads clamp to MAX_QUADS");
    assert_eq!(args, [MAX_QUADS * 6, 1, 0, 0], "indirect args clamp to MAX_QUADS");

    let mut remaining: HashMap<Quad, u32> = HashMap::new();
    for q in &cpu {
        *remaining.entry(*q).or_default() += 1;
    }
    for q in &gpu {
        let n = remaining.get_mut(q).unwrap_or_else(|| panic!("GPU emitted a quad absent from the CPU oracle: {q:?}"));
        assert!(*n > 0, "GPU emitted a quad more often than the CPU oracle: {q:?}");
        *n -= 1;
    }
}

/// Create a headless device, or `None` if the machine has no usable GPU.
fn init_gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let limits = adapter.limits();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("mesher-test"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}
