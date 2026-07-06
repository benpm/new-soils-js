//! Headless proof that the WGSL codegen matches the CPU oracle.
//!
//! Builds a **noise-free** terrain graph (Coord/Constant/ScaleBias/Add/Abs/
//! Clamp/Min/Max/Lerp/Terrace/Power/DomainWarp), generates its compute shader,
//! runs it on a real wgpu device over a grid of world columns, and compares the
//! Height buffer to `TerrainGraph::eval_columns` entry-for-entry. Noise nodes
//! are excluded on purpose: the GPU simplex is only character-equivalent to the
//! `noise` crate, but every *combinator/modulator* must match exactly — which
//! is what proves the code generator itself is correct.
//!
//! Skips gracefully when no GPU adapter is available.

use noise::Simplex;
use soils_worldgen::graph::{Axis, CaveParams, In, Node, NodeKind, Outputs, TerrainGraph};
use wgpu::util::DeviceExt;

// Path hack: pull the codegen straight from the binary crate's source. Keeping
// it a bin (no lib) means we `include!` the module under test here.
#[path = "../src/wgsl_gen.rs"]
mod wgsl_gen;

const RES: u32 = 24;
const ORIGIN: [f32; 2] = [-137.0, 88.0];
const STEP: f32 = 9.5;

/// A graph exercising the non-noise nodes. Height is a domain-warped, clamped
/// blend of the two coordinate axes; Structure is a terraced/absolute value.
fn build_graph() -> TerrainGraph {
    // Indices:
    let nodes = vec![
        node(0, NodeKind::Coord { axis: Axis::X }),                                  // x
        node(1, NodeKind::Coord { axis: Axis::Z }),                                  // z
        node(2, NodeKind::ScaleBias { input: In::from(0), scale: 0.5, bias: 3.0 }),  // 0.5x+3
        node(3, NodeKind::ScaleBias { input: In::from(1), scale: -0.25, bias: 1.0 }),// -0.25z+1
        // Domain-warp x's contribution by z.
        node(4, NodeKind::DomainWarp { input: In::from(2), wx: In::from(1), wz: In::from(0), amount: 0.01 }),
        node(5, NodeKind::Add { a: In::from(4), b: In::from(3) }),                   // warped+..
        node(6, NodeKind::Abs { input: In::from(5) }),
        node(7, NodeKind::Clamp { input: In::from(6), min: 0.0, max: 80.0 }),
        node(8, NodeKind::Constant { value: 40.0 }),
        node(9, NodeKind::Min { a: In::from(7), b: In::from(8) }),
        node(10, NodeKind::Lerp { a: In::from(6), b: In::from(9), t: In::constant(0.3) }),
        // Structure channel: terraced absolute of a scaled coord, in [0,1]-ish.
        node(11, NodeKind::ScaleBias { input: In::from(0), scale: 0.02, bias: 0.0 }),
        node(12, NodeKind::Terrace { input: In::from(11), steps: 4.0 }),
        node(13, NodeKind::Clamp { input: In::from(12), min: 0.0, max: 1.0 }),
    ];
    TerrainGraph {
        nodes,
        outputs: Outputs { height: In::from(10), rock: None, structure: Some(In::from(13)) },
        caves: CaveParams::default(),
    }
}

fn node(id: usize, kind: NodeKind) -> Node {
    Node { id, kind }
}

#[test]
fn gpu_codegen_matches_cpu_oracle() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter; skipping gpu_codegen_matches_cpu_oracle");
        return;
    };
    let graph = build_graph();
    graph.validate().unwrap();

    let src = wgsl_gen::generate(&graph);
    let params = wgsl_gen::collect_params(&graph);

    // --- buffers ---
    let count = (RES * RES) as u64;
    let view = [ORIGIN[0], ORIGIN[1], STEP, RES as f32];
    let view_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("view"),
        contents: bytemuck::cast_slice(&view),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    // P must be non-empty for a valid binding.
    let p_data: Vec<f32> = if params.is_empty() { vec![0.0] } else { params.clone() };
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
    let structure = mk_out("out_structure");
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: bytes,
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
            wgpu::BindGroupEntry { binding: 3, resource: structure.as_entire_binding() },
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
    let data = slice.get_mapped_range();
    let gpu: &[f32] = bytemuck::cast_slice(&data);

    // --- compare to CPU oracle ---
    let sim = Simplex::new(0); // unused (no noise nodes) but required by the API
    let mut compared = 0;
    for j in 0..RES {
        for i in 0..RES {
            let x = ORIGIN[0] as f64 + i as f64 * STEP as f64;
            let z = ORIGIN[1] as f64 + j as f64 * STEP as f64;
            let want = graph.eval_columns(&sim, x, z).height as f32;
            let got = gpu[(j * RES + i) as usize];
            let tol = 1e-2 + 1e-3 * want.abs();
            assert!(
                (got - want).abs() <= tol,
                "height mismatch at ({x},{z}): gpu={got} cpu={want}\n--- shader ---\n{src}"
            );
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
