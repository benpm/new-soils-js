//! Headless proof that the GPU light flood (`assets/shaders/light_flood.wgsl`)
//! converges to the exact `soils_sim::light` fixed point: mini pooled caches
//! go in, reseed + beam + relax passes run to convergence, and every chunk's
//! light volume is byte-compared against `relight_full` on a CPU `LightWorld`
//! with the client's domain semantics (unloaded space is out of domain, open
//! sky above the domain). Also covers the incremental edit schedule (3×3×3
//! reseed + column below) against `apply_voxel_change`.
//!
//! Skips gracefully if no GPU adapter is available.

use std::collections::HashMap;

use glam::IVec3;
use soils_protocol::{CHUNK_SIZE, ChunkVolume, chunk_of, local_of};
use soils_sim::light::{self, ChunkLight, LightWorld};
use wgpu::util::DeviceExt;

const EMITTER_ID: u8 = 7;
const EMITTER_LEVEL: u8 = 12;

struct TestWorld {
    chunks: HashMap<IVec3, (ChunkVolume, ChunkLight)>,
}

impl TestWorld {
    fn voxel(&self, v: IVec3) -> u8 {
        match self.chunks.get(&chunk_of(v)) {
            Some((vol, _)) => {
                let l = local_of(v);
                vol.get(l.x, l.y, l.z)
            }
            None => 0,
        }
    }
}

impl LightWorld for TestWorld {
    fn solid(&self, v: IVec3) -> bool {
        self.voxel(v) != 0
    }
    fn emission(&self, v: IVec3) -> u8 {
        if self.voxel(v) == EMITTER_ID { EMITTER_LEVEL } else { 0 }
    }
    fn light(&self, v: IVec3) -> u8 {
        match self.chunks.get(&chunk_of(v)) {
            Some((_, l)) => {
                let p = local_of(v);
                l.get(p.x, p.y, p.z)
            }
            None => 0,
        }
    }
    fn set_light(&mut self, v: IVec3, packed: u8) {
        if let Some((_, l)) = self.chunks.get_mut(&chunk_of(v)) {
            let p = local_of(v);
            l.set(p.x, p.y, p.z, packed);
        }
    }
    fn in_domain(&self, v: IVec3) -> bool {
        self.chunks.contains_key(&chunk_of(v))
    }
    fn open_sky_above(&self, _v: IVec3) -> bool {
        true
    }
}

/// GPU-side mini world: pools sized to the mapped set, slot i per chunk in
/// insertion order, mesh slot i+1 (0 is the air sentinel).
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    reseed: wgpu::ComputePipeline,
    beam: wgpu::ComputePipeline,
    relax: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl Gpu {
    fn init() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("light-test"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .ok()?;
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/shaders/light_flood.wgsl"
        ))
        .expect("read light_flood.wgsl");
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("light_flood"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let entry = |binding, read_only| wgpu::BindGroupLayoutEntry {
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
            label: Some("light_layout"),
            entries: &[
                entry(0, false),
                entry(1, true),
                entry(2, true),
                entry(3, true),
                entry(4, true),
                entry(5, true),
                entry(6, true),
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
        Some(Self {
            reseed: pipe("reseed"),
            beam: pipe("beam"),
            relax: pipe("relax"),
            layout,
            device,
            queue,
        })
    }

    /// Flood `core` (order given) with `relax_set` participating in relax, and
    /// return every mapped chunk's 32³ light bytes.
    fn flood(
        &self,
        world: &TestWorld,
        order: &[IVec3],
        core: &[IVec3],
        prior: Option<&HashMap<IVec3, Vec<u8>>>,
    ) -> HashMap<IVec3, Vec<u8>> {
        self.flood_with_points(world, order, core, prior, &[])
    }

    /// As [`Gpu::flood`], with non-block emitters (the player light) standing
    /// in the given air voxels at the given levels.
    fn flood_with_points(
        &self,
        world: &TestWorld,
        order: &[IVec3],
        core: &[IVec3],
        prior: Option<&HashMap<IVec3, Vec<u8>>>,
        points: &[(IVec3, u8)],
    ) -> HashMap<IVec3, Vec<u8>> {
        let mapped: Vec<IVec3> = order.to_vec();
        let slot_of = |c: IVec3| mapped.iter().position(|&m| m == c).map(|i| i as u32);
        let n = mapped.len();

        // Buffers.
        let mut light = vec![0u8; n * 32768];
        if let Some(prior) = prior {
            for (i, c) in mapped.iter().enumerate() {
                light[i * 32768..(i + 1) * 32768].copy_from_slice(&prior[c]);
            }
        }
        let mut voxels = vec![0u8; (n + 1) * 32768];
        for (i, c) in mapped.iter().enumerate() {
            voxels[(i + 1) * 32768..(i + 2) * 32768]
                .copy_from_slice(world.chunks[c].0.as_bytes());
        }
        let mut desc = vec![0i32; n * 8];
        for (i, c) in mapped.iter().enumerate() {
            desc[i * 8] = c.x;
            desc[i * 8 + 1] = c.y;
            desc[i * 8 + 2] = c.z;
            desc[i * 8 + 3] = (i + 1) as i32; // mesh slot
        }
        let mut table = vec![u32::MAX; 32 * 32 * 32];
        for (i, c) in mapped.iter().enumerate() {
            let idx = ((c.x & 31) + (c.y & 31) * 32 + (c.z & 31) * 1024) as usize;
            table[idx] = i as u32;
        }
        let mut emitters = vec![0u32; 256];
        emitters[EMITTER_ID as usize] = EMITTER_LEVEL as u32;
        // Jobs: core (reseed/beam), then the full mapped set for relax.
        let job_bytes = |list: &[IVec3]| -> Vec<u8> {
            let mut b = Vec::new();
            for c in list {
                let slot = slot_of(*c).expect("core chunks are mapped");
                for v in [c.x, c.y, c.z, slot as i32, (slot + 1) as i32, 0, 0, 0] {
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
            b
        };
        let mut core_sorted = core.to_vec();
        core_sorted.sort_by_key(|c| std::cmp::Reverse(c.y));
        let core_b = job_bytes(&core_sorted);
        let relax_b = job_bytes(&mapped);

        let mk = |label: &str, contents: &[u8], usage| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            })
        };
        let st = wgpu::BufferUsages::STORAGE;
        let light_buf = mk("light", &light, st | wgpu::BufferUsages::COPY_SRC);
        let vox_buf = mk("voxels", &voxels, st);
        let desc_buf = mk("desc", bytemuck::cast_slice(&desc), st);
        let table_buf = mk("table", bytemuck::cast_slice(&table), st);
        let emit_buf = mk("emitters", bytemuck::cast_slice(&emitters), st);
        // Always at least one row: a zero-sized storage binding is invalid, and
        // level 0 is the disabled row the shader skips.
        let mut point_rows: Vec<i32> = Vec::new();
        for (v, level) in points {
            point_rows.extend_from_slice(&[v.x, v.y, v.z, i32::from(*level)]);
        }
        if point_rows.is_empty() {
            point_rows.extend_from_slice(&[0, 0, 0, 0]);
        }
        let points_buf = mk("point_lights", bytemuck::cast_slice(&point_rows), st);
        let core_buf = mk("core_jobs", &core_b, st);
        let relax_buf = mk("relax_jobs", &relax_b, st);

        let bg = |jobs: &wgpu::Buffer| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: light_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: vox_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: desc_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: table_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 4, resource: emit_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 5, resource: jobs.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 6, resource: points_buf.as_entire_binding() },
                ],
            })
        };
        let core_bg = bg(&core_buf);
        let relax_bg = bg(&relax_buf);

        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (n * 32768) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            let cn = core_sorted.len() as u32;
            pass.set_bind_group(0, &core_bg, &[]);
            pass.set_pipeline(&self.reseed);
            pass.dispatch_workgroups(128, cn, 1);
            pass.set_pipeline(&self.beam);
            let layers: std::collections::HashSet<i32> =
                core_sorted.iter().map(|c| c.y).collect();
            for _ in 0..layers.len().max(1) {
                pass.dispatch_workgroups(4, cn, 1);
            }
            pass.set_bind_group(0, &relax_bg, &[]);
            pass.set_pipeline(&self.relax);
            // Generous rounds: correctness at convergence is what's asserted.
            for _ in 0..48 {
                pass.dispatch_workgroups(128, n as u32, 1);
            }
        }
        enc.copy_buffer_to_buffer(&light_buf, 0, &readback, 0, (n * 32768) as u64);
        self.queue.submit([enc.finish()]);

        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let data = slice.get_mapped_range();
        let mut out = HashMap::new();
        for (i, c) in mapped.iter().enumerate() {
            out.insert(*c, data[i * 32768..(i + 1) * 32768].to_vec());
        }
        drop(data);
        readback.unmap();
        out
    }
}

fn assert_matches(name: &str, world: &TestWorld, gpu: &HashMap<IVec3, Vec<u8>>) {
    for (c, (_, cl)) in &world.chunks {
        assert_eq!(
            gpu[c].as_slice(),
            // ChunkLight is Uniform|Dense now; a uniform chunk has no byte
            // array to borrow, so the oracle comparison materializes one.
            cl.as_dense_bytes().as_ref(),
            "{name}: chunk {c:?} light differs from the CPU oracle"
        );
    }
}

fn world_from(chunks: Vec<(IVec3, ChunkVolume)>) -> (TestWorld, Vec<IVec3>) {
    let order: Vec<IVec3> = chunks.iter().map(|(c, _)| *c).collect();
    let world = TestWorld {
        chunks: chunks
            .into_iter()
            .map(|(c, v)| (c, (v, ChunkLight::dark())))
            .collect(),
    };
    (world, order)
}

#[test]
fn gpu_flood_matches_relight_full() {
    let Some(gpu) = Gpu::init() else {
        eprintln!("no GPU adapter available; skipping gpu_flood_matches_relight_full");
        return;
    };

    // Scene A: one chunk — heightmap with an overhang pocket + an emitter in
    // a sealed room.
    let mut a = ChunkVolume::empty();
    for x in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            let h = 8 + ((x / 5) * 2 + (z / 7) * 3) % 10;
            for y in 0..h {
                a.set(x, y, z, 1);
            }
        }
    }
    // Overhang: a plate with air under it.
    for x in 4..12 {
        for z in 4..12 {
            a.set(x, 20, z, 1);
        }
    }
    // Sealed room with an emitter.
    for x in 20..28 {
        for y in 22..28 {
            for z in 20..28 {
                a.set(x, y, z, 1);
            }
        }
    }
    for x in 21..27 {
        for y in 23..27 {
            for z in 21..27 {
                a.set(x, y, z, 0);
            }
        }
    }
    a.set(23, 23, 23, EMITTER_ID);

    // Scene B/C: horizontal pair with a doorway in the shared wall, under a
    // stacked chunk that blocks half the sky (beam + lateral cross-border).
    let mut b = ChunkVolume::empty();
    for y in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            b.set(31, y, z, 1); // wall at the +x border
        }
    }
    for y in 4..10 {
        for z in 12..20 {
            b.set(31, y, z, 0); // doorway
        }
    }
    let c = ChunkVolume::empty();
    let mut top = ChunkVolume::empty();
    for x in 0..CHUNK_SIZE {
        for z in 0..16 {
            top.set(x, 5, z, 2); // half-roof over chunk B
        }
    }

    let (mut world, order) = world_from(vec![
        (IVec3::new(0, 0, 0), a),
        (IVec3::new(4, 0, 0), b),
        (IVec3::new(5, 0, 0), c),
        (IVec3::new(4, 1, 0), top),
    ]);

    let chunks: Vec<IVec3> = order.clone();
    light::relight_full(&mut world, &chunks);
    let got = gpu.flood(&world, &order, &order, None);
    assert_matches("initial", &world, &got);

    // Incremental: knock a hole in the half-roof (light-adding edit), then
    // seal the doorway (light-removing edit). CPU applies apply_voxel_change;
    // GPU re-floods the edit's 3×3×3 ∩ mapped plus the columns below, with
    // everything mapped relaxing — the client's exact schedule.
    let mut apply_edit = |world: &mut TestWorld, v: IVec3, id: u8, prior: &HashMap<IVec3, Vec<u8>>| {
        let cpos = chunk_of(v);
        let l = local_of(v);
        world.chunks.get_mut(&cpos).unwrap().0.set(l.x, l.y, l.z, id);
        light::apply_voxel_change(world, v);
        // GPU schedule: 3×3×3 core + mapped columns below.
        let mut core = Vec::new();
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let n = cpos + IVec3::new(dx, dy, dz);
                    if world.chunks.contains_key(&n) && !core.contains(&n) {
                        core.push(n);
                        let mut below = n - IVec3::Y;
                        while world.chunks.contains_key(&below) {
                            if !core.contains(&below) {
                                core.push(below);
                            }
                            below.y -= 1;
                        }
                    }
                }
            }
        }
        gpu.flood(world, &order, &core, Some(prior))
    };

    let got = apply_edit(&mut world, IVec3::new(4 * 32 + 8, 32 + 5, 8), 0, &got);
    assert_matches("roof hole edit", &world, &got);

    let mut got2 = HashMap::new();
    for v in [
        IVec3::new(4 * 32 + 31, 6, 14), // partially reseal the doorway
        IVec3::new(4 * 32 + 31, 7, 14),
    ] {
        got2 = apply_edit(&mut world, v, 1, if got2.is_empty() { &got } else { &got2 });
    }
    assert_matches("doorway seal edits", &world, &got2);
}
/// The player is a light source: an emitter that is not a block, stamped into
/// the grid by the reseed pass (see `PlayerLight`). It has to behave exactly
/// like a placed emitter — full level in its own cell, one level lost per step,
/// and stopped by geometry — because the whole point of doing it in the flood
/// rather than in the terrain shader is that walls occlude it.
#[test]
fn a_point_light_lights_air_and_is_occluded() {
    let Some(gpu) = Gpu::init() else {
        eprintln!("no GPU adapter available; skipping a_point_light_lights_air_and_is_occluded");
        return;
    };

    // A sealed room, so no skylight reaches in and every lit cell is the
    // point light's doing. A wall splits it, with the light on one side.
    let mut vol = ChunkVolume::empty();
    for x in 4..28 {
        for y in 4..28 {
            for z in 4..28 {
                vol.set(x, y, z, 1);
            }
        }
    }
    for x in 5..27 {
        for y in 5..27 {
            for z in 5..27 {
                vol.set(x, y, z, 0);
            }
        }
    }
    for y in 5..27 {
        for z in 5..27 {
            vol.set(16, y, z, 1); // full-height partition at x = 16
        }
    }

    let (world, order) = world_from(vec![(IVec3::ZERO, vol)]);
    let level = 12u8;
    let src = IVec3::new(10, 16, 16);
    let got = gpu.flood_with_points(&world, &order, &order, None, &[(src, level)]);
    let block_at = |v: IVec3| light::block(got[&IVec3::ZERO][light_index(v)]);

    assert_eq!(block_at(src), level, "the emitter cell holds its own level");
    for step in 1..=5i32 {
        assert_eq!(
            block_at(src + IVec3::new(0, 0, step)),
            level - step as u8,
            "one level lost per step, {step} away"
        );
    }
    // Past the partition: the only path is through solid rock, so nothing.
    assert_eq!(block_at(IVec3::new(20, 16, 16)), 0, "the wall must stop it");
    // And the far side of the room it *is* in stays reachable, so the zero
    // above is occlusion rather than the light simply not spreading.
    assert!(block_at(IVec3::new(14, 16, 16)) > 0, "same side of the wall stays lit");
}

/// Level 0 is the disabled row the binding always carries. It must light
/// nothing, or "player light off" would still glow.
#[test]
fn a_zero_level_point_light_lights_nothing() {
    let Some(gpu) = Gpu::init() else {
        eprintln!("no GPU adapter available; skipping a_zero_level_point_light_lights_nothing");
        return;
    };
    let mut vol = ChunkVolume::empty();
    for x in 4..28 {
        for y in 4..28 {
            for z in 4..28 {
                vol.set(x, y, z, 1);
            }
        }
    }
    for x in 5..27 {
        for y in 5..27 {
            for z in 5..27 {
                vol.set(x, y, z, 0);
            }
        }
    }
    let (world, order) = world_from(vec![(IVec3::ZERO, vol)]);
    let src = IVec3::new(10, 16, 16);
    let got = gpu.flood_with_points(&world, &order, &order, None, &[(src, 0)]);
    assert_eq!(light::block(got[&IVec3::ZERO][light_index(src)]), 0);
}

/// Index of a chunk-local voxel in a flood readback volume.
fn light_index(v: IVec3) -> usize {
    (v.x + v.y * 32 + v.z * 1024) as usize
}
