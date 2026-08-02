//! Headless proof that the fixed-point WGSL codegen is **bit-exact** with the
//! CPU evaluator — noise nodes included (worldgen v2's core guarantee: chunk
//! generation is the same pure function of (seed, graph, world_type, pos) on
//! every CPU and GPU).
//!
//! Builds a graph exercising every supported node kind, generates its column
//! compute shader, runs it on a real wgpu device over a grid of world columns,
//! and compares all three output channels to `CompiledGraph::columns_fx`
//! entry-for-entry with exact integer equality.
//!
//! Skips gracefully when no GPU adapter is available.

use soils_worldgen::graph::{Axis, CaveParams, In, Node, NodeKind, Outputs, TerrainGraph};
use soils_worldgen::wgsl;
use wgpu::util::DeviceExt;

const RES: u32 = 24;
const ORIGIN: [i32; 2] = [-137, 88];
const STEP: i32 = 9;
const SEED: u32 = 12345;

/// A graph exercising every v2-supported node kind, noise included. Height is
/// a domain-warped, clamped blend of octave noise and the coordinate axes;
/// Rock is plain noise; Structure is a terraced absolute value.
fn build_graph() -> TerrainGraph {
    let nodes = vec![
        node(0, NodeKind::Coord { axis: Axis::X }),
        node(1, NodeKind::Coord { axis: Axis::Z }),
        node(2, NodeKind::Simplex2 { frequency: 1.0 / 100.0, offset: [3.25, -7.5] }),
        node(3, NodeKind::Fbm {
            octaves: 4,
            base_frequency: 1.0 / 500.0,
            lacunarity: 2.0,
            persistence: 0.5,
            offset: [0.0, 11.0],
        }),
        node(4, NodeKind::ScaleBias { input: In::from(3), scale: 40.0, bias: 100.0 }),
        node(5, NodeKind::ScaleBias { input: In::from(0), scale: 0.01, bias: 0.0 }),
        // Domain-warp the fbm terrain by raw noise.
        node(6, NodeKind::DomainWarp {
            input: In::from(4),
            wx: In::from(2),
            wz: In::from(2),
            amount: 20.0,
        }),
        node(7, NodeKind::Add { a: In::from(6), b: In::from(5) }),
        node(8, NodeKind::Abs { input: In::from(7) }),
        node(9, NodeKind::Clamp { input: In::from(8), min: 0.0, max: 300.0 }),
        node(10, NodeKind::Constant { value: 240.0 }),
        node(11, NodeKind::Min { a: In::from(9), b: In::from(10) }),
        node(12, NodeKind::Max { a: In::from(11), b: In::constant(-50.0) }),
        node(13, NodeKind::Lerp { a: In::from(8), b: In::from(12), t: In::constant(0.3) }),
        node(14, NodeKind::Sub { a: In::from(13), b: In::constant(1.5) }),
        // Structure channel: terraced scaled coord in [0,1]-ish.
        node(15, NodeKind::ScaleBias { input: In::from(1), scale: 0.02, bias: 0.0 }),
        node(16, NodeKind::Terrace { input: In::from(15), steps: 4.0 }),
        node(17, NodeKind::Clamp { input: In::from(16), min: 0.0, max: 1.0 }),
        // Rock: another seeded-offset noise, scaled.
        node(18, NodeKind::Simplex2 { frequency: 1.0 / 15.0, offset: [0.0, 0.0] }),
        node(19, NodeKind::Mul { a: In::from(18), b: In::constant(5.0) }),
    ];
    TerrainGraph {
        nodes,
        outputs: Outputs {
            height: In::from(14),
            rock: Some(In::from(19)),
            structure: Some(In::from(17)),
        },
        caves: CaveParams::default(),
    }
}

fn node(id: usize, kind: NodeKind) -> Node {
    Node { id, kind }
}

#[test]
fn gpu_codegen_matches_cpu_bit_exactly() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter; skipping gpu_codegen_matches_cpu_bit_exactly");
        return;
    };
    let graph = build_graph();
    graph.validate().unwrap();

    let src = wgsl::generate_columns(&graph).unwrap();
    let params = wgsl::collect_params(&graph);

    // --- buffers ---
    let count = (RES * RES) as u64;
    let view = [ORIGIN[0], ORIGIN[1], STEP, RES as i32, SEED as i32, 0, 0, 0];
    let view_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("view"),
        contents: bytemuck::cast_slice(&view),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let p_data: Vec<i32> = if params.is_empty() { vec![0] } else { params.clone() };
    let p_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("P"),
        contents: bytemuck::cast_slice(&p_data),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let bytes = count * 4;
    let mk_out = |label| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    };
    let height = mk_out("out_height");
    let rock = mk_out("out_rock");
    let structure = mk_out("out_structure");
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: bytes * 3,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // --- pipeline ---
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("terrain-codegen"),
        source: wgpu::ShaderSource::Wgsl(src.clone().into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("terrain"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bg"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: view_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: p_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: height.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: rock.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: structure.as_entire_binding() },
        ],
    });

    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let groups = RES.div_ceil(8);
        pass.dispatch_workgroups(groups, groups, 1);
    }
    enc.copy_buffer_to_buffer(&height, 0, &readback, 0, bytes);
    enc.copy_buffer_to_buffer(&rock, 0, &readback, bytes, bytes);
    enc.copy_buffer_to_buffer(&structure, 0, &readback, bytes * 2, bytes);
    queue.submit([enc.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let gpu: &[i32] = bytemuck::cast_slice(&data);
    let n = (RES * RES) as usize;

    // --- compare to the CPU evaluator, exact integer equality ---
    let compiled = graph.compile().unwrap();
    let mut compared = 0;
    for j in 0..RES {
        for i in 0..RES {
            let x = ORIGIN[0] + i as i32 * STEP;
            let z = ORIGIN[1] + j as i32 * STEP;
            let (h, r, st) = compiled.columns_fx(SEED, x.wrapping_shl(16), z.wrapping_shl(16));
            let idx = (j * RES + i) as usize;
            assert_eq!(
                gpu[idx], h,
                "height mismatch at ({x},{z}): gpu={} cpu={h}\n--- shader ---\n{src}",
                gpu[idx]
            );
            assert_eq!(gpu[n + idx], r, "rock mismatch at ({x},{z})");
            assert_eq!(gpu[2 * n + idx], st, "structure mismatch at ({x},{z})");
            compared += 1;
        }
    }
    assert!(compared > 0);
    drop(data);
    readback.unmap();
}

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
        label: Some("codegen-test"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}
