//! Greedy mesher, ported from `mesher_worker.js`.
//!
//! Like the JS worker, this operates on a single chunk's `32^3` voxel buffer and
//! treats anything outside the chunk as Air, so faces on chunk borders are always
//! emitted (matching the original's behavior).
//!
//! The 3-axis sweep with a signed mask is ported closely from the JS. A `merge`
//! flag controls quad merging: with `merge = false` every exposed face becomes a
//! 1×1 quad, which keeps atlas UVs un-stretched for the simple StandardMaterial
//! path. Ambient occlusion from the JS version is intentionally omitted for now.

use soils_protocol::{CHUNK_SIZE, ChunkVolume};

/// Geometry produced for one chunk. Positions/normals are per-vertex; `block_ids`
/// is per-quad (one entry per two triangles), so the client can pick atlas tiles.
#[derive(Debug, Default, Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    pub block_ids: Vec<u8>,
}

impl MeshData {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Run the greedy sweep over a chunk volume.
pub fn greedy_mesh(vol: &ChunkVolume, merge: bool) -> MeshData {
    let dims = [CHUNK_SIZE, CHUNK_SIZE, CHUNK_SIZE];
    let mut out = MeshData::default();

    // Voxel lookup; callers only ever pass in-range coordinates here.
    let query = |i: i32, j: i32, k: i32| -> i32 { vol.get(i, j, k) as i32 };

    let mut mask = vec![0i32; (dims[0] * dims[1]) as usize];

    for d in 0..3usize {
        let u = (d + 1) % 3;
        let v = (d + 2) % 3;
        let mut x = [0i32; 3];
        let mut q = [0i32; 3];
        q[d] = 1;

        let mask_len = (dims[u] * dims[v]) as usize;
        if mask.len() < mask_len {
            mask.resize(mask_len, 0);
        }

        x[d] = -1;
        while x[d] < dims[d] {
            // --- Compute the mask for this slice. ---
            let mut n = 0usize;
            x[v] = 0;
            while x[v] < dims[v] {
                x[u] = 0;
                while x[u] < dims[u] {
                    let a = if 0 <= x[d] { query(x[0], x[1], x[2]) } else { 0 };
                    let b = if x[d] < dims[d] - 1 {
                        query(x[0] + q[0], x[1] + q[1], x[2] + q[2])
                    } else {
                        0
                    };
                    mask[n] = if (a != 0) == (b != 0) {
                        0
                    } else if a != 0 {
                        a
                    } else {
                        -b
                    };
                    n += 1;
                    x[u] += 1;
                }
                x[v] += 1;
            }

            x[d] += 1;

            // --- Generate quads from the mask. ---
            n = 0;
            let mut j = 0i32;
            while j < dims[v] {
                let mut i = 0i32;
                while i < dims[u] {
                    let mut c = mask[n];
                    if c != 0 {
                        // Quad width along u.
                        let mut width = 1i32;
                        if merge {
                            while i + width < dims[u] && mask[n + width as usize] == c {
                                width += 1;
                            }
                        }
                        // Quad height along v.
                        let mut height = 1i32;
                        if merge {
                            'h: while j + height < dims[v] {
                                let mut k = 0i32;
                                while k < width {
                                    if mask[n + (k + height * dims[u]) as usize] != c {
                                        break 'h;
                                    }
                                    k += 1;
                                }
                                height += 1;
                            }
                        }

                        x[u] = i;
                        x[v] = j;
                        let mut du = [0i32; 3];
                        let mut dv = [0i32; 3];
                        let norm: [f32; 3];

                        if c > 0 {
                            dv[v] = height;
                            du[u] = width;
                            norm = unit(d, 1.0);
                        } else {
                            c = -c;
                            du[v] = height;
                            dv[u] = width;
                            norm = unit(d, -1.0);
                        }

                        let base = out.positions.len() as u32;
                        out.positions.push(fv(x));
                        out.positions.push(fv([x[0] + du[0], x[1] + du[1], x[2] + du[2]]));
                        out.positions.push(fv([
                            x[0] + du[0] + dv[0],
                            x[1] + du[1] + dv[1],
                            x[2] + du[2] + dv[2],
                        ]));
                        out.positions.push(fv([x[0] + dv[0], x[1] + dv[1], x[2] + dv[2]]));
                        for _ in 0..4 {
                            out.normals.push(norm);
                        }
                        out.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                        out.block_ids.push((c & 255) as u8);

                        // Zero out the consumed mask region.
                        for l in 0..height {
                            for k in 0..width {
                                mask[n + (k + l * dims[u]) as usize] = 0;
                            }
                        }

                        i += width;
                        n += width as usize;
                    } else {
                        i += 1;
                        n += 1;
                    }
                }
                j += 1;
            }
        }
    }

    out
}

#[inline]
fn unit(axis: usize, sign: f32) -> [f32; 3] {
    let mut n = [0.0f32; 3];
    n[axis] = sign;
    n
}

#[inline]
fn fv(v: [i32; 3]) -> [f32; 3] {
    [v[0] as f32, v[1] as f32, v[2] as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_voxel_has_six_quads() {
        let mut vol = ChunkVolume::empty();
        vol.set(5, 5, 5, 1);
        let mesh = greedy_mesh(&vol, false);
        // 6 faces -> 6 quads -> 24 verts, 36 indices.
        assert_eq!(mesh.block_ids.len(), 6);
        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
    }

    #[test]
    fn empty_chunk_makes_no_geometry() {
        let vol = ChunkVolume::empty();
        assert!(greedy_mesh(&vol, true).is_empty());
    }

    #[test]
    fn merge_reduces_quad_count() {
        // A 4x1x4 slab of one block: top/bottom should merge into 1 quad each
        // when merging is enabled, vs 16 each when not.
        let mut vol = ChunkVolume::empty();
        for x in 0..4 {
            for z in 0..4 {
                vol.set(x, 0, z, 1);
            }
        }
        let merged = greedy_mesh(&vol, true);
        let unmerged = greedy_mesh(&vol, false);
        assert!(merged.block_ids.len() < unmerged.block_ids.len());
    }
}
