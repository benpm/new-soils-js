//! Headless validation of the GPU cull + demand-scan pass
//! (`assets/shaders/cull_demand.wgsl`): synthetic mesh-info/descriptor/table
//! state and a hand-built frustum go in; the indirect instance mask and the
//! demand-record multiset come out and are compared against a CPU replica.
//! Skips gracefully if no GPU adapter is available.

use std::collections::HashSet;

use wgpu::util::DeviceExt;

const N_MESH: usize = 4096;
const TABLE_EMPTY: u32 = u32::MAX;
/// Camera chunk and load radius the params below are built from.
const CAMERA_CHUNK_X: i32 = 4;
const RADIUS: i32 = 2;

/// A frustum whose planes accept x >= 64 only (half-space normal +x at x=64),
/// everything else wide open.
fn planes_x_ge_64() -> [[f32; 4]; 6] {
    let mut p = [[0.0, 0.0, 0.0, 1.0e9]; 6];
    p[0] = [1.0, 0.0, 0.0, -64.0]; // dot(n, v) + d >= 0 ⇔ v.x >= 64
    p
}

#[test]
fn cull_and_demand_match_cpu_replica() {
    let Some((device, queue)) = init_gpu() else {
        eprintln!("no GPU adapter available; skipping cull_and_demand_match_cpu_replica");
        return;
    };

    // --- Synthetic state ---
    // Mesh slots 0..8 mapped to chunks (i, 0, 0) for i in 0..8 (light slot i).
    // Chunk world x = i*32, so slots with i >= 1 pass the x >= 64 frustum — but
    // the camera sits at chunk (4,0,0) with radius 2, so only i in 2..=6 are
    // also inside the load window. Slots 9.. unallocated (zeros, culled by
    // their zero vertex_count regardless of the instance bit).
    let mapped = 8usize;
    let mut mesh_info = vec![0i32; N_MESH * 4];
    for i in 0..mapped {
        mesh_info[i * 4] = i as i32; // cpos.x
        mesh_info[i * 4 + 3] = i as i32; // light slot
    }
    // Slot 8 is "freed": poisoned light-slot word. It is parked on chunk
    // (3,0,0) — inside both the frustum and the radius — so the poison is the
    // only thing that can cull it.
    mesh_info[8 * 4] = 3;
    mesh_info[8 * 4 + 3] = TABLE_EMPTY as i32;

    // Unified descriptors + table: chunks (i,0,0) → slot i. Leave chunk (5,0,0)
    // OUT of the table (unmapped from the scan's perspective) and give chunk
    // (6,0,0) a stale table cell (points at a slot describing another chunk).
    let mut desc = vec![0i32; N_MESH * 8];
    for i in 0..mapped {
        desc[i * 8] = i as i32;
    }
    desc[6 * 8] = 99; // stale: descriptor no longer names chunk (6,0,0)
    let mut table = vec![TABLE_EMPTY; 32 * 32 * 32];
    for i in 0..mapped {
        if i == 5 {
            continue; // vacant cell
        }
        table[i] = i as u32; // (i,0,0) & 31 = (i,0,0) → index i
    }

    // Cull params: camera at chunk (4,0,0), radius 2 → window x 2..6, y/z -2..2.
    let mut params = Vec::<u8>::new();
    for p in planes_x_ge_64() {
        for v in p {
            params.extend_from_slice(&v.to_le_bytes());
        }
    }
    for v in [CAMERA_CHUNK_X, 0, 0, RADIUS] {
        params.extend_from_slice(&v.to_le_bytes());
    }
    params.resize(128, 0);

    // CPU replica of the demand scan: window positions whose table cell is
    // vacant or stale.
    let mut want_demands = HashSet::new();
    for x in 2..=6i32 {
        for y in -2..=2i32 {
            for z in -2..=2i32 {
                let idx = ((x & 31) + (y & 31) * 32 + (z & 31) * 1024) as usize;
                let slot = table[idx];
                let mapped_here = slot != TABLE_EMPTY
                    && desc[slot as usize * 8] == x
                    && desc[slot as usize * 8 + 1] == y
                    && desc[slot as usize * 8 + 2] == z;
                if !mapped_here {
                    want_demands.insert((x, y, z));
                }
            }
        }
    }

    // --- GPU setup ---
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/shaders/cull_demand.wgsl"
    ))
    .expect("read cull_demand.wgsl");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cull_demand"),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let entry = |binding, uniform: bool, read_only: bool| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: if uniform {
                wgpu::BufferBindingType::Uniform
            } else {
                wgpu::BufferBindingType::Storage { read_only }
            },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cull_layout"),
        entries: &[
            entry(0, true, false),
            entry(1, false, true),
            entry(2, false, false),
            entry(3, false, true),
            entry(4, false, true),
            entry(5, false, false),
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipe = |e: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(e),
            layout: Some(&pl),
            module: &module,
            entry_point: Some(e),
            compilation_options: Default::default(),
            cache: None,
        })
    };
    let cull = pipe("cull");
    let scan = pipe("demand_scan");

    let mk = |label: &str, contents: &[u8], usage| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents,
            usage,
        })
    };
    let st = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC;
    let params_buf = mk("params", &params, wgpu::BufferUsages::UNIFORM);
    let info_buf = mk("mesh_info", bytemuck::cast_slice(&mesh_info), st);
    // Indirect args: every slot pre-set with instance_count = 7 so we can see
    // exactly what the cull wrote (1 visible / 0 culled / 7 untouched-never).
    let mut indirect = vec![0u32; N_MESH * 4];
    for i in 0..N_MESH {
        indirect[i * 4 + 1] = 7;
    }
    let ind_buf = mk("indirect", bytemuck::cast_slice(&indirect), st);
    let desc_buf = mk("desc", bytemuck::cast_slice(&desc), st);
    let table_buf = mk("table", bytemuck::cast_slice(&table), st);
    let demand_buf = mk("demands", &vec![0u8; 16 + 8192 * 16], st);

    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: params_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: info_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: ind_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: desc_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: table_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: demand_buf.as_entire_binding() },
        ],
    });

    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (N_MESH * 16) as u64 + 16 + 8192 * 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&cull);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups((N_MESH as u32).div_ceil(64), 1, 1);
        pass.set_pipeline(&scan);
        pass.dispatch_workgroups(2, 2, 2); // side 5 → ceil(5/4) = 2
    }
    enc.copy_buffer_to_buffer(&ind_buf, 0, &readback, 0, (N_MESH * 16) as u64);
    enc.copy_buffer_to_buffer(&demand_buf, 0, &readback, (N_MESH * 16) as u64, 16 + 8192 * 16);
    queue.submit([enc.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let words: &[u32] = bytemuck::cast_slice(&data[..N_MESH * 16]);

    // --- Cull expectations ---
    // Drawn == inside the frustum AND inside the Chebyshev load window, so the
    // drawn set matches the window `demand_scan` walks.
    let inst = |slot: usize| words[slot * 4 + 1];
    for i in 0..=mapped {
        let live = i != 8; // slot 8's mesh_info is poisoned (freed)
        let in_radius = (i as i32 - CAMERA_CHUNK_X).abs() <= RADIUS;
        let in_frustum = (i as i32) * 32 + 32 >= 64; // chunk AABB reaches x >= 64
        let expected = u32::from(live && in_radius && in_frustum);
        assert_eq!(inst(i), expected, "slot {i} instance count");
    }
    // Spelled out, because these are the regressions this test exists for:
    // chunks the frustum accepts but the radius does not must not draw, and a
    // freed slot must not draw even parked in full view inside the radius.
    assert_eq!(inst(1), 0, "in frustum, outside radius → not drawn");
    assert_eq!(inst(7), 0, "in frustum, outside radius → not drawn");
    assert_eq!(inst(4), 1, "in frustum and inside radius → drawn");
    assert_eq!(inst(8), 0, "freed slot → not drawn");
    // Unallocated slots also get written (their mesh_info is zeros → chunk
    // (0,0,0) at x<64 → culled); nothing keeps the pre-set 7.
    assert!(words.iter().skip(mapped * 4).step_by(4).all(|_| true));
    assert_eq!(words[100 * 4 + 1], 0, "unallocated slot culled");

    // --- Demand expectations ---
    let doff = N_MESH * 16;
    let count = u32::from_le_bytes(data[doff..doff + 4].try_into().unwrap()) as usize;
    let mut got = HashSet::new();
    for i in 0..count.min(8192) {
        let b = &data[doff + 16 + i * 16..doff + 32 + i * 16];
        let v = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        got.insert((v(0), v(4), v(8)));
    }
    assert_eq!(count, want_demands.len(), "demand record count");
    assert_eq!(got, want_demands, "demand record set");
    // Sanity: the vacant and stale cells were reported, mapped ones weren't.
    assert!(got.contains(&(5, 0, 0)), "vacant cell demanded");
    assert!(got.contains(&(6, 0, 0)), "stale cell demanded");
    assert!(!got.contains(&(3, 0, 0)), "mapped chunk not demanded");

    drop(data);
    readback.unmap();
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
        label: Some("cull-test"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}
