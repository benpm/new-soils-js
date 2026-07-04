//! Headless GPU validation of the radiance-cascades compute shader.
//!
//! Runs `assets/shaders/radiance.wgsl`'s `trace` pass on a real wgpu device
//! (no window or surface needed) over a controlled voxel scene, reads the
//! cascade-0 buffer back, and asserts it matches the CPU oracle in
//! `soils-worldgen::radiance` entry-for-entry. This is what proves the GPU port
//! of the ray-march (voxel unpacking, octahedral directions, emission lookup,
//! interval math) is correct — the merge equation itself is unit-tested on the
//! CPU side. Skips gracefully if no GPU adapter is available.

use soils_worldgen::radiance::{self, LightGrid, Radiance};
use wgpu::util::DeviceExt;

// Must match radiance.wgsl / gi.rs.
const GI_DIM: i32 = 64;
const C0_PROBES: u32 = 16;
const C0_DIRRES: u32 = 4;
const C0_SPACING: f32 = 4.0;
const C0_INT: (f32, f32) = (0.0, 2.0);
const STEP: f32 = 0.5;

const EMITTER_ID: u8 = 5;
const EMISSION: [f32; 3] = [2.0, 3.0, 4.0];

fn cascade0_entries() -> u32 {
    C0_PROBES * C0_PROBES * C0_PROBES * C0_DIRRES * C0_DIRRES
}

/// Build the scene shared by GPU and CPU: a block of emitter voxels next to the
/// first probe so several cascade-0 rays terminate on it.
fn build_scene() -> (Vec<u8>, LightGrid) {
    let dim = GI_DIM;
    let mut bytes = vec![0u8; (dim * dim * dim) as usize];
    let mut grid = LightGrid::new(dim);
    for x in 3..=5 {
        for y in 0..=4 {
            for z in 0..=4 {
                let idx = ((y * dim + z) * dim + x) as usize;
                bytes[idx] = EMITTER_ID;
                grid.set_solid(x, y, z, EMISSION);
            }
        }
    }
    (bytes, grid)
}

/// CPU oracle radiance for one cascade-0 (probe, direction) entry.
fn cpu_entry(grid: &LightGrid, px: u32, py: u32, pz: u32, dx: u32, dy: u32) -> Radiance {
    let probe = [
        (px as f32 + 0.5) * C0_SPACING,
        (py as f32 + 0.5) * C0_SPACING,
        (pz as f32 + 0.5) * C0_SPACING,
    ];
    let dir = radiance::dir_for_texel(dx, dy, C0_DIRRES);
    radiance::trace_interval(grid, probe, dir, C0_INT.0, C0_INT.1, STEP)
}

fn entry_index(px: u32, py: u32, pz: u32, dx: u32, dy: u32) -> usize {
    let dirs = C0_DIRRES * C0_DIRRES;
    let pidx = (py * C0_PROBES + pz) * C0_PROBES + px;
    let didx = dy * C0_DIRRES + dx;
    (pidx * dirs + didx) as usize
}

#[test]
fn gpu_trace_matches_cpu_oracle() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter available; skipping gpu_trace_matches_cpu_oracle");
        return;
    };

    let (vox_bytes, grid) = build_scene();

    // --- GPU buffers ---
    let world_vox = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("world_vox"),
        contents: &vox_bytes,
        usage: wgpu::BufferUsages::STORAGE,
    });
    // Emission table: vec4<f32> per block id (id EMITTER_ID glows).
    let mut emission = vec![0.0f32; (EMITTER_ID as usize + 1) * 4];
    emission[EMITTER_ID as usize * 4] = EMISSION[0];
    emission[EMITTER_ID as usize * 4 + 1] = EMISSION[1];
    emission[EMITTER_ID as usize * 4 + 2] = EMISSION[2];
    let emission_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("emission"),
        contents: bytemuck::cast_slice(&emission),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let cascade_bytes = cascade0_entries() as u64 * 16;
    let cascade = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cascade0"),
        size: cascade_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    // params: origin(3)+day, zenith(3)+pad, horizon(3)+enabled = 12 f32.
    let params: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 0.5, 0.7, 1.0, 0.0, 0.8, 0.85, 0.9, 1.0];
    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::cast_slice(&params),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let meta_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("meta"),
        contents: bytemuck::cast_slice(&[0u32]),
        usage: wgpu::BufferUsages::STORAGE,
    });
    // `far` is unused by trace but can't alias the read-write cascade buffer.
    let dummy = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dummy_far"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: cascade_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // --- pipeline ---
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/shaders/radiance.wgsl"
    ))
    .expect("read radiance.wgsl");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("radiance"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });

    let ro = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let mut entries: Vec<_> = (0..6).map(ro).collect();
    entries[2].ty = wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only: false },
        has_dynamic_offset: false,
        min_binding_size: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("gi_layout"),
        entries: &entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("gi_pl"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("trace"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("trace"),
        compilation_options: Default::default(),
        cache: None,
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("gi_bg"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: world_vox.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: emission_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: cascade.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: params_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: meta_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: dummy.as_entire_binding() },
        ],
    });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass =
            encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(cascade0_entries().div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&cascade, 0, &readback, 0, cascade_bytes);
    queue.submit([encoder.finish()]);

    // --- read back ---
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let gpu: &[f32] = bytemuck::cast_slice(&data);

    // Compare a broad sample of entries against the CPU oracle, and confirm the
    // emitter is actually seen (some rays terminate on it) rather than all-open.
    let mut terminal_near_emitter = 0;
    let mut compared = 0;
    for (px, py, pz) in [(0u32, 0u32, 0u32), (1, 0, 0), (0, 1, 0), (3, 3, 3), (5, 5, 5)] {
        for dy in 0..C0_DIRRES {
            for dx in 0..C0_DIRRES {
                let want = cpu_entry(&grid, px, py, pz, dx, dy);
                let i = entry_index(px, py, pz, dx, dy) * 4;
                let got = [gpu[i], gpu[i + 1], gpu[i + 2], gpu[i + 3]];
                assert!(
                    (got[0] - want.rgb[0]).abs() < 1e-3
                        && (got[1] - want.rgb[1]).abs() < 1e-3
                        && (got[2] - want.rgb[2]).abs() < 1e-3,
                    "rgb mismatch at probe({px},{py},{pz}) dir({dx},{dy}): gpu={got:?} cpu={:?}",
                    want.rgb
                );
                assert!(
                    (got[3] - want.vis).abs() < 1e-3,
                    "vis mismatch at probe({px},{py},{pz}) dir({dx},{dy}): gpu={} cpu={}",
                    got[3],
                    want.vis
                );
                if (px, py, pz) == (0, 0, 0) && want.vis < 0.5 {
                    terminal_near_emitter += 1;
                }
                compared += 1;
            }
        }
    }
    assert!(compared > 0);
    assert!(
        terminal_near_emitter > 0,
        "no cascade-0 ray from the first probe hit the emitter — trace produced nothing"
    );

    drop(data);
    readback.unmap();
}

/// The GPU occupancy blit (`gi_blit.wgsl`) must produce exactly the volume
/// the old CPU fill built: chunk bytes land at their chunk-aligned offset,
/// everything else cleared to air.
#[test]
fn gpu_blit_matches_cpu_fill() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter available; skipping gpu_blit_matches_cpu_fill");
        return;
    };

    // A deterministic chunk pattern (chunk layout: (y + z*32)*32 + x).
    let mut chunk = vec![0u8; 32 * 32 * 32];
    let mut s = 7u64;
    for b in chunk.iter_mut() {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (s >> 33) as u8;
    }
    let rels = [[0i32, 0, 0], [32, 32, 0]];

    // CPU reference (volume layout: (y*64 + z)*64 + x).
    let dim = GI_DIM as usize;
    let mut want = vec![0u8; dim * dim * dim];
    for rel in rels {
        for y in 0..32usize {
            for z in 0..32usize {
                for x in 0..32usize {
                    let v = chunk[(y + z * 32) * 32 + x];
                    let (vx, vy, vz) =
                        (x + rel[0] as usize, y + rel[1] as usize, z + rel[2] as usize);
                    want[(vy * dim + vz) * dim + vx] = v;
                }
            }
        }
    }

    let chunk_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("chunk"),
        contents: &chunk,
        usage: wgpu::BufferUsages::STORAGE,
    });
    let vol_bytes = (dim * dim * dim) as u64;
    let world_vox = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("world_vox"),
        size: vol_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: vol_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/shaders/gi_blit.wgsl"
    ))
    .expect("read gi_blit.wgsl");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("gi_blit"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let ro = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let mut entries: Vec<_> = (0..3).map(ro).collect();
    entries[1].ty = wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only: false },
        has_dynamic_offset: false,
        min_binding_size: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("blit_layout"),
        entries: &entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("blit_pl"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let make = |entry: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        })
    };
    let clear = make("clear_volume");
    let blit = make("blit_chunk");

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        for (i, rel) in rels.iter().enumerate() {
            let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blit_params"),
                contents: bytemuck::cast_slice(&[rel[0], rel[1], rel[2], 0]),
                usage: wgpu::BufferUsages::STORAGE,
            });
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: chunk_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: world_vox.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
                ],
            });
            if i == 0 {
                pass.set_pipeline(&clear);
                pass.set_bind_group(0, &bg, &[]);
                pass.dispatch_workgroups((vol_bytes as u32 / 4).div_ceil(64), 1, 1);
            }
            pass.set_pipeline(&blit);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(1, 8, 8);
        }
    }
    encoder.copy_buffer_to_buffer(&world_vox, 0, &readback, 0, vol_bytes);
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    assert_eq!(&data[..], &want[..], "GPU blit volume differs from the CPU fill reference");
    drop(data);
    readback.unmap();
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
    // Use the adapter's real limits: the layout binds 6 storage buffers, more
    // than the conservative downlevel cap of 4.
    let limits = adapter.limits();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("gi-test"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}
