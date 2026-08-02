//! CPU-side slot allocation for the pooled GPU chunk cache (stream-pipeline
//! redesign). Two slot spaces: every resident chunk (air included — sky light
//! crosses air chunks) owns a *unified* slot for its light volume + descriptor;
//! non-air chunks additionally own a *mesh* slot for voxels/quads/indirect
//! args. The GPU never allocates — systems allocate here and emit buffer
//! writes, so residency policy stays in one place.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::renderer::RenderDevice;
use bevy::render::settings::WgpuFeatures;
use bevy::render::{ExtractSchedule, MainWorld, RenderApp, RenderStartup};

/// Unified slots (light + descriptor). Covers a full r8 window (17³ = 4913)
/// with headroom for hysteresis and in-flight loads.
pub const N_SLOTS: u32 = 6144;
/// Mesh slots (voxels + quads + indirect). Only non-air chunks need one
/// (~2.5k at r8). Slot 0 is a permanently-zero sentinel so air chunks' voxel
/// reads resolve to air without branching; it is never handed out.
pub const N_MESH: u32 = 4096;
/// `Slot::mesh` value for air chunks (no mesh slot).
pub const NO_MESH: u32 = u32::MAX;
/// Slot-table axis size: chunk coord masked to `& 31` per axis. Collision-free
/// while the active window is < 32 chunks per axis (radius ≤ 8 → 17), but
/// entries left behind by movement go stale, so every lookup — GPU and CPU —
/// validates the resolved slot's chunk position.
pub const TABLE_DIM: i32 = 32;
/// Empty slot-table entry.
pub const TABLE_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub slot: u32,
    /// Mesh-space index, or [`NO_MESH`] for air chunks.
    pub mesh: u32,
}

/// Index into the wrap-window slot table for a chunk position.
pub fn table_index(c: IVec3) -> usize {
    let m = TABLE_DIM - 1;
    ((c.x & m) + (c.y & m) * TABLE_DIM + (c.z & m) * TABLE_DIM * TABLE_DIM) as usize
}

#[derive(Resource)]
pub struct ChunkSlots {
    map: HashMap<IVec3, Slot>,
    free: Vec<u32>,
    free_mesh: Vec<u32>,
}

impl Default for ChunkSlots {
    fn default() -> Self {
        Self {
            map: HashMap::default(),
            // Popped from the back; low indices hand out first for stable tests.
            free: (0..N_SLOTS).rev().collect(),
            // Mesh slot 0 is the air sentinel — never in the free list.
            free_mesh: (1..N_MESH).rev().collect(),
        }
    }
}

impl ChunkSlots {
    /// Allocate a slot for `pos`. Idempotent: an already-resident chunk gets
    /// its existing slot back (upgraded with a mesh slot if newly needed).
    /// `None` = pool exhausted; the caller defers the chunk and retries (the
    /// demand queue re-issues records every frame).
    pub fn alloc(&mut self, pos: IVec3, needs_mesh: bool) -> Option<Slot> {
        if self.map.contains_key(&pos) {
            return if needs_mesh { self.ensure_mesh(pos).map(|_| self.map[&pos]) } else { Some(self.map[&pos]) };
        }
        let slot = self.free.pop()?;
        let mesh = if needs_mesh {
            match self.free_mesh.pop() {
                Some(m) => m,
                None => {
                    self.free.push(slot);
                    return None;
                }
            }
        } else {
            NO_MESH
        };
        let s = Slot { slot, mesh };
        self.map.insert(pos, s);
        Some(s)
    }

    /// Give a resident air chunk a mesh slot (first non-air edit). `None` =
    /// not resident or mesh pool exhausted.
    pub fn ensure_mesh(&mut self, pos: IVec3) -> Option<u32> {
        let s = self.map.get_mut(&pos)?;
        if s.mesh != NO_MESH {
            return Some(s.mesh);
        }
        s.mesh = self.free_mesh.pop()?;
        Some(s.mesh)
    }

    /// Release `pos`'s slots (unload/eviction). Returns what was freed so the
    /// caller can invalidate the descriptor + slot table on the GPU.
    pub fn free(&mut self, pos: IVec3) -> Option<Slot> {
        let s = self.map.remove(&pos)?;
        self.free.push(s.slot);
        if s.mesh != NO_MESH {
            self.free_mesh.push(s.mesh);
        }
        Some(s)
    }

    pub fn get(&self, pos: IVec3) -> Option<Slot> {
        self.map.get(&pos).copied()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &Slot)> {
        self.map.iter()
    }

    /// Drop everything (warp: the whole world went away).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// What the adapter actually gave us, probed once at render startup. Phase 3
/// sizes the pools from this (and refuses to start with a clear error instead
/// of a cryptic wgpu validation panic if the device can't fit them). Bevy's
/// default `WgpuSettingsPriority::Functionality` already requests the full
/// adapter feature/limit set, so nothing needs forcing at device creation.
/// `multi_draw_indirect` itself is always callable in wgpu 27 (emulated as a
/// `draw_indirect` loop without native support); `native_multi_draw` records
/// whether `MULTI_DRAW_INDIRECT_COUNT` says it is a real single submission.
#[derive(Resource, Clone, Copy, Debug)]
pub struct GpuCaps {
    pub native_multi_draw: bool,
    pub max_storage_binding: u64,
    pub max_buffer_size: u64,
}

fn probe_gpu_caps(device: Res<RenderDevice>, mut commands: Commands) {
    let features = device.features();
    let limits = device.limits();
    let caps = GpuCaps {
        native_multi_draw: features.contains(WgpuFeatures::MULTI_DRAW_INDIRECT_COUNT),
        max_storage_binding: limits.max_storage_buffer_binding_size as u64,
        max_buffer_size: limits.max_buffer_size,
    };
    info!(
        "gpu caps: native_multi_draw={} max_storage_binding={}MiB max_buffer={}MiB",
        caps.native_multi_draw,
        caps.max_storage_binding >> 20,
        caps.max_buffer_size >> 20
    );
    commands.insert_resource(caps);
}

/// Mirrors [`GpuCaps`] into the main world so pool sizing (main-world systems)
/// can read it.
fn mirror_caps(caps: Option<Res<GpuCaps>>, mut main: ResMut<MainWorld>) {
    if let Some(caps) = caps
        && !main.contains_resource::<GpuCaps>()
    {
        main.insert_resource(*caps);
    }
}

pub struct PoolPlugin;

impl Plugin for PoolPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChunkSlots>();
        let render_app = app.sub_app_mut(RenderApp);
        render_app.add_systems(RenderStartup, probe_gpu_caps);
        render_app.add_systems(ExtractSchedule, mirror_caps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_roundtrip() {
        let mut s = ChunkSlots::default();
        let a = s.alloc(IVec3::new(1, 2, 3), true).unwrap();
        assert_ne!(a.mesh, NO_MESH);
        assert_ne!(a.mesh, 0, "mesh sentinel must never be handed out");
        let b = s.alloc(IVec3::new(4, 5, 6), false).unwrap();
        assert_eq!(b.mesh, NO_MESH);
        assert_ne!(a.slot, b.slot);
        assert_eq!(s.len(), 2);
        assert_eq!(s.free(IVec3::new(1, 2, 3)), Some(a));
        assert_eq!(s.free(IVec3::new(1, 2, 3)), None);
        assert_eq!(s.len(), 1);
        // Freed indices come back around.
        let c = s.alloc(IVec3::new(7, 8, 9), true).unwrap();
        assert_eq!((c.slot, c.mesh), (a.slot, a.mesh));
    }

    #[test]
    fn alloc_is_idempotent() {
        let mut s = ChunkSlots::default();
        let a = s.alloc(IVec3::ZERO, false).unwrap();
        assert_eq!(s.alloc(IVec3::ZERO, false), Some(a));
        // Upgrading with needs_mesh attaches a mesh slot, same unified slot.
        let b = s.alloc(IVec3::ZERO, true).unwrap();
        assert_eq!(b.slot, a.slot);
        assert_ne!(b.mesh, NO_MESH);
        assert_eq!(s.alloc(IVec3::ZERO, true), Some(b));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn ensure_mesh_upgrades_air() {
        let mut s = ChunkSlots::default();
        s.alloc(IVec3::ONE, false).unwrap();
        let m = s.ensure_mesh(IVec3::ONE).unwrap();
        assert_ne!(m, NO_MESH);
        assert_eq!(s.get(IVec3::ONE).unwrap().mesh, m);
        assert_eq!(s.ensure_mesh(IVec3::ONE), Some(m));
        assert_eq!(s.ensure_mesh(IVec3::splat(9)), None);
    }

    #[test]
    fn exhaustion_returns_none_and_leaks_nothing() {
        let mut s = ChunkSlots::default();
        // Drain the mesh pool (N_MESH - 1 real slots).
        for i in 0..(N_MESH - 1) as i32 {
            assert!(s.alloc(IVec3::new(i, 0, 0), true).is_some(), "i={i}");
        }
        let before = s.len();
        // Mesh pool empty: mesh alloc fails and must return the unified slot.
        assert_eq!(s.alloc(IVec3::new(-1, -1, -1), true), None);
        assert_eq!(s.len(), before);
        // Air chunks still allocate until the unified pool runs dry.
        for i in 0..(N_SLOTS - N_MESH + 1) as i32 {
            assert!(s.alloc(IVec3::new(i, 1, 0), false).is_some(), "i={i}");
        }
        assert_eq!(s.alloc(IVec3::new(-2, -2, -2), false), None);
    }

    #[test]
    fn table_index_wraps() {
        assert_eq!(table_index(IVec3::ZERO), 0);
        assert_eq!(table_index(IVec3::new(32, 0, 0)), 0);
        assert_eq!(table_index(IVec3::new(-1, 0, 0)), 31);
        assert_eq!(table_index(IVec3::new(1, 1, 1)), (1 + 32 + 1024) as usize);
        // Distinct within a 17-chunk window.
        assert_ne!(table_index(IVec3::new(8, 0, 0)), table_index(IVec3::new(-8, 0, 0)));
    }

    #[test]
    fn clear_resets() {
        let mut s = ChunkSlots::default();
        s.alloc(IVec3::ONE, true).unwrap();
        s.clear();
        assert_eq!(s.len(), 0);
        assert_eq!(s.get(IVec3::ONE), None);
    }
}
