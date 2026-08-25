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

use soils_worldgen::graph::{Axis, CaveParams, In, Node, NodeKind, NoiseMode, Outputs, TerrainGraph};
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

/// The design-tool f32 hash-noise nodes are NOT bit-exact (which is why
/// `TerrainGraph::deterministic` rejects them from the game path), but the CPU
/// and GPU ports come from the same source and must agree to within f32
/// rounding — that is what makes the tool's 3D preview match its 2D map.
/// Sums every *continuous* mode plus a fractal stack into Height and compares
/// with a small Q16.16 tolerance.
///
/// The discontinuous `Wavelet` mode is deliberately excluded: its per-cell
/// random rotation means a 1-ULP coordinate difference flips a cell at
/// boundaries and the value can differ by ~0.02 there. That is intrinsic to
/// the algorithm, not a porting bug; it is covered by the CPU
/// `modes_are_finite_and_bounded` test and visual screenshots.
#[test]
fn hash_noise_gpu_matches_cpu_within_tolerance() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter; skipping hash_noise_gpu_matches_cpu_within_tolerance");
        return;
    };
    // Non-round frequencies keep sample coords off exact integer cell
    // boundaries; varied offsets decorrelate the modes.
    let mut nodes: Vec<Node> = [
        (NoiseMode::Value, 0.037, [0.0, 0.0]),
        (NoiseMode::Perlin, 0.041, [11.0, -7.0]),
        (NoiseMode::Simplex, 0.043, [-2.0, 9.0]),
        (NoiseMode::Worley, 0.033, [3.0, 5.0]),
        (NoiseMode::Voronoi, 0.031, [0.0, 0.0]),
        (NoiseMode::Gabor, 0.029, [0.0, 0.0]),
        (NoiseMode::Crater, 0.027, [1.0, -4.0]),
        (NoiseMode::Wool, 0.023, [0.0, 0.0]),
        (NoiseMode::Stone, 0.021, [6.0, 2.0]),
    ]
    .into_iter()
    .enumerate()
    .map(|(id, (mode, frequency, offset))| {
        node(id, NodeKind::Noise { mode, frequency, offset, param: 0.4 })
    })
    .collect();
    nodes.push(node(9, NodeKind::FractalNoise {
        mode: NoiseMode::Perlin,
        octaves: 4,
        base_frequency: 0.019,
        lacunarity: 2.0,
        persistence: 0.5,
        offset: [4.0, -1.0],
        param: 0.5,
    }));
    for i in 0..9 {
        // Adds 10..=18 fold sources 1..=9 onto source 0; height = node 18.
        let prev = if i == 0 { 0 } else { 9 + i };
        nodes.push(node(10 + i, NodeKind::Add { a: In::from(prev), b: In::from(i + 1) }));
    }
    let graph = TerrainGraph {
        nodes,
        outputs: Outputs { height: In::from(18), rock: None, structure: None },
        caves: CaveParams::default(),
    };
    graph.validate().unwrap();
    // Design-only nodes must be rejected from the deterministic game path.
    assert!(graph.deterministic().is_err());

    let src = wgsl::generate_columns(&graph).unwrap();
    let params = wgsl::collect_params(&graph);
    let gpu = run_columns(&device, &queue, &src, &params);

    let compiled = graph.compile().unwrap();
    // ~0.015 in Q16.16: room for fma contraction and the f32-vs-f64 rounding
    // at the fx boundary across 10 summed sources, far below the O(0.1..2)
    // error of a real porting slip.
    const TOL: i32 = 1000;
    for j in 0..RES {
        for i in 0..RES {
            let x = ORIGIN[0] + i as i32 * STEP;
            let z = ORIGIN[1] + j as i32 * STEP;
            let (h, _, _) = compiled.columns_fx(SEED, x.wrapping_shl(16), z.wrapping_shl(16));
            let got = gpu[(j * RES + i) as usize];
            assert!(
                (got - h).abs() <= TOL,
                "noise height mismatch at ({x},{z}): gpu={} cpu={} ({} vs {})",
                got,
                h,
                got as f32 / 65536.0,
                h as f32 / 65536.0,
            );
        }
    }
}

/// Dispatch the columns kernel for `src`/`params` and read back the Height
/// channel (the second test's harness; the first test keeps its three-channel
/// readback inline).
fn run_columns(device: &wgpu::Device, queue: &wgpu::Queue, src: &str, params: &[i32]) -> Vec<i32> {
    let count = (RES * RES) as u64;
    let view = [ORIGIN[0], ORIGIN[1], STEP, RES as i32, SEED as i32, 0, 0, 0];
    let view_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("view"),
        contents: bytemuck::cast_slice(&view),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let p_data: Vec<i32> = if params.is_empty() { vec![0] } else { params.to_vec() };
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
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("terrain-codegen"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
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
    queue.submit([enc.finish()]);
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let out = {
        let data = slice.get_mapped_range();
        bytemuck::cast_slice::<u8, i32>(&data).to_vec()
    };
    readback.unmap();
    out
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
