//! Headless proof that the GPU chunk-generation kernel is **bit-exact** with
//! `TerrainGen::generate` — the worldgen v2 guarantee that lets GPU-born
//! chunks be authoritative. Runs `gen_lattice` + `gen_fill` for chunks in
//! every cost class (surface, deep-cave, sky, negative coords, flat world)
//! and byte-compares the full 32³ volume.
//!
//! Skips gracefully when no GPU adapter is available.

use glam::IVec3;
use soils_worldgen::{TerrainGraph, TerrainGen, WorldType, default_registry, wgsl};
use wgpu::util::DeviceExt;

const SEED: u32 = 424242;

#[test]
fn gpu_chunk_gen_matches_cpu_bit_exactly() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter; skipping gpu_chunk_gen_matches_cpu_bit_exactly");
        return;
    };
    let reg = default_registry();
    let graph = TerrainGraph::default_soils();
    let src = wgsl::generate_chunk(&graph).unwrap();
    let params = wgsl::collect_params(&graph);

    // Palette word packing must match CHUNK_HEADER's layout.
    let id = |name: &str| reg.id_of(name).unwrap_or(0) as u32;
    let pal0 = id("Grass") | (id("Slate") << 8) | (id("Stone") << 16) | (id("Rocky Dirt") << 24);
    let pal1 = id("Tough Dirt") | (id("Dirt") << 8);

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("gen-chunk"),
        source: wgpu::ShaderSource::Wgsl(src.clone().into()),
    });
    // Explicit shared layout: implicit (`layout: None`) layouts contain only
    // the bindings each entry point uses, so the two entry points would get
    // incompatible bind groups.
    let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("gen-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            storage(1, true),
            storage(2, false),
            storage(3, false),
            storage(4, true),
            storage(5, false),
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("gen-layout"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let mk_pipeline = |entry: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(&layout),
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        })
    };
    let lattice_pipe = mk_pipeline("gen_lattice");
    let fill_pipe = mk_pipeline("gen_fill");

    let p_data: Vec<i32> = if params.is_empty() { vec![0] } else { params.clone() };
    let p_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("P"),
        contents: bytemuck::cast_slice(&p_data),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let lattice = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("lattice"),
        size: 9 * 9 * 9 * 4,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let voxels = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("voxels"),
        size: 32 * 32 * 32,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: 32 * 32 * 32 + 8,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let cases: &[(IVec3, WorldType)] = &[
        (IVec3::new(0, 8, 0), WorldType::Normal),   // surface band
        (IVec3::new(2, 8, -3), WorldType::Normal),  // surface, negative coords
        (IVec3::new(6, 4, 7), WorldType::Normal),   // deep: every voxel pays cave noise
        (IVec3::new(8, 14, 8), WorldType::Normal),  // sky: all air
        (IVec3::new(-5, 7, 12), WorldType::Normal),
        (IVec3::new(0, 8, 0), WorldType::Flat),     // flat surface chunk
        (IVec3::new(0, 7, 0), WorldType::Flat),     // flat below-surface chunk
    ];

    for &(cpos, world_type) in cases {
        let origin = cpos * 32;
        let flags: u32 = if world_type == WorldType::Flat { 1 } else { 0 };
        let view = [flags, SEED, pal0, pal1];
        let view_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("gen-view"),
            contents: bytemuck::cast_slice(&view),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // One job: this chunk into slot 0.
        let jobs_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("jobs"),
            contents: bytemuck::cast_slice(&[origin.x, origin.y, origin.z, 0]),
            usage: wgpu::BufferUsages::STORAGE,
        });
        // [batch tag, job 0 non-air count] — zeroed per case.
        let occ_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("occ"),
            contents: &[0u8; 8],
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: view_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: p_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: lattice.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: voxels.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: jobs_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: occ_buf.as_entire_binding() },
            ],
        });

        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&lattice_pipe);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&fill_pipe);
            pass.dispatch_workgroups(1, 4, 1);
        }
        enc.copy_buffer_to_buffer(&voxels, 0, &readback, 0, 32 * 32 * 32);
        enc.copy_buffer_to_buffer(&occ_buf, 0, &readback, 32 * 32 * 32, 8);
        queue.submit([enc.finish()]);

        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let gpu_bytes = slice.get_mapped_range().to_vec();
        readback.unmap();

        let tg = TerrainGen::new(SEED, world_type);
        let want = tg.generate(cpos, &reg);
        assert_eq!(
            gpu_bytes[..32 * 32 * 32].as_ref() as &[u8],
            want.as_bytes(),
            "chunk {cpos:?} ({world_type:?}) differs\n--- shader ---\n{src}"
        );
        let occ = u32::from_le_bytes(gpu_bytes[32 * 32 * 32 + 4..].try_into().unwrap());
        let cpu_occ = want.as_bytes().iter().filter(|b| **b != 0).count() as u32;
        assert_eq!(occ, cpu_occ, "chunk {cpos:?} occupancy count differs");
    }
}

fn init_gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let limits = adapter.limits();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("gen-test"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}
