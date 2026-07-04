//! Greedy mesher, ported from `mesher_worker.js`.
//!
//! Like the JS worker, this operates on a single chunk's `32^3` voxel buffer and
//! treats anything outside the chunk as Air, so faces on chunk borders are always
//! emitted (matching the original's behavior).
//!
//! The 3-axis sweep with a signed mask is ported closely from the JS. Per-vertex
//! ambient occlusion is computed with the canonical 3-sample corner formula (the
//! same `occlusion()` the JS used). Merging is **AO-aware**: two faces only merge
//! when both their block id and their four corner AO levels match, so flat areas
//! collapse into big quads without smearing AO across them.

use soils_protocol::{CHUNK_SIZE, ChunkVolume};

/// Geometry produced for one chunk. Positions/normals/ao are per-vertex;
/// `block_ids` is per-quad (one entry per two triangles) so the client can pick
/// atlas tiles.
#[derive(Debug, Default, Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    pub block_ids: Vec<u8>,
    /// Per-vertex ambient-occlusion brightness in `[0.1, 1.0]` (1.0 = unoccluded).
    pub ao: Vec<f32>,
}

impl MeshData {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// The four face-corner positions in `(u, v)` order, matching the vertex push
/// order `[origin, +du, +du+dv, +dv]`.
const CORNER_UV: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];
/// Neighbor offsets used to gather the side/corner occluders for a vertex.
const AO_OFFSETS: [(i32, i32); 4] = [(-1, 0), (-1, -1), (0, -1), (0, 0)];

#[inline]
fn occlusion(side1: bool, side2: bool, corner: bool) -> i32 {
    if side1 && side2 {
        0
    } else {
        3 - (side1 as i32 + side2 as i32 + corner as i32)
    }
}

/// Run the greedy sweep over a chunk volume.
pub fn greedy_mesh(vol: &ChunkVolume, merge: bool) -> MeshData {
    let dims = [CHUNK_SIZE, CHUNK_SIZE, CHUNK_SIZE];
    let mut out = MeshData::default();

    let query = |i: i32, j: i32, k: i32| -> i32 { vol.get(i, j, k) as i32 };
    // Bounds-checked occupancy for AO; outside the chunk counts as Air.
    let solid = |p: [i32; 3]| -> bool {
        p[0] >= 0
            && p[0] < CHUNK_SIZE
            && p[1] >= 0
            && p[1] < CHUNK_SIZE
            && p[2] >= 0
            && p[2] < CHUNK_SIZE
            && vol.get(p[0], p[1], p[2]) != 0
    };

    let mut mask = vec![0i32; (dims[0] * dims[1]) as usize];
    // Per-cell AO: comparison key (occlusion levels) + emit brightness.
    let mut ao_key = vec![[0u8; 4]; mask.len()];
    let mut ao_bright = vec![[1.0f32; 4]; mask.len()];

    for d in 0..3usize {
        let u = (d + 1) % 3;
        let v = (d + 2) % 3;
        let mut x = [0i32; 3];
        let mut q = [0i32; 3];
        q[d] = 1;

        let mask_len = (dims[u] * dims[v]) as usize;
        if mask.len() < mask_len {
            mask.resize(mask_len, 0);
            ao_key.resize(mask_len, [0u8; 4]);
            ao_bright.resize(mask_len, [1.0f32; 4]);
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

            // --- Compute per-cell ambient occlusion for this slice's faces. ---
            n = 0;
            for j in 0..dims[v] {
                for i in 0..dims[u] {
                    let c = mask[n];
                    if c != 0 {
                        let positive = c > 0;
                        let mut norm = [0i32; 3];
                        norm[d] = if positive { 1 } else { -1 };
                        // In-plane basis (cx, cy), mirroring the emit step.
                        let (mut cx, mut cy) = ([0i32; 3], [0i32; 3]);
                        if positive {
                            cy[(d + 2) % 3] = 1;
                            cx[(d + 1) % 3] = 1;
                        } else {
                            cx[(d + 2) % 3] = 1;
                            cy[(d + 1) % 3] = 1;
                        }
                        let mut base = [0i32; 3];
                        base[d] = x[d];
                        base[u] = i;
                        base[v] = j;

                        for (w, &(a, b)) in CORNER_UV.iter().enumerate() {
                            let vpos = [
                                base[0] + cx[0] * a + cy[0] * b,
                                base[1] + cx[1] * a + cy[1] * b,
                                base[2] + cx[2] * a + cy[2] * b,
                            ];
                            let at = |o: (i32, i32)| -> bool {
                                solid([
                                    vpos[0] + norm[0] + cx[0] * o.0 + cy[0] * o.1,
                                    vpos[1] + norm[1] + cx[1] * o.0 + cy[1] * o.1,
                                    vpos[2] + norm[2] + cx[2] * o.0 + cy[2] * o.1,
                                ])
                            };
                            let s1 = at(AO_OFFSETS[w]);
                            let s2 = at(AO_OFFSETS[(w + 2) % 4]);
                            let cc = at(AO_OFFSETS[(w + 1) % 4]);
                            let level = occlusion(s1, s2, cc);
                            ao_key[n][w] = level as u8;
                            ao_bright[n][w] = 0.1 + level as f32 * 0.3;
                        }
                    }
                    n += 1;
                }
            }

            // --- Generate quads from the mask (AO-aware merging). ---
            n = 0;
            let mut j = 0i32;
            while j < dims[v] {
                let mut i = 0i32;
                while i < dims[u] {
                    let mut c = mask[n];
                    if c != 0 {
                        let base_key = ao_key[n];
                        // Quad width along u: same block id and same corner AO.
                        let mut width = 1i32;
                        if merge {
                            while i + width < dims[u]
                                && mask[n + width as usize] == c
                                && ao_key[n + width as usize] == base_key
                            {
                                width += 1;
                            }
                        }
                        // Quad height along v.
                        let mut height = 1i32;
                        if merge {
                            'h: while j + height < dims[v] {
                                let mut k = 0i32;
                                while k < width {
                                    let idx = n + (k + height * dims[u]) as usize;
                                    if mask[idx] != c || ao_key[idx] != base_key {
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
                        // AO is uniform across the merged region, so use the base
                        // cell's four corner values in vertex order.
                        let bright = ao_bright[n];
                        out.ao.extend_from_slice(&bright);
                        out.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                        out.block_ids.push((c & 255) as u8);

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
        assert_eq!(mesh.block_ids.len(), 6);
        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
        assert_eq!(mesh.ao.len(), 24);
    }

    #[test]
    fn empty_chunk_makes_no_geometry() {
        let vol = ChunkVolume::empty();
        assert!(greedy_mesh(&vol, true).is_empty());
    }

    #[test]
    fn merge_reduces_quad_count() {
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

    #[test]
    fn winding_matches_normal() {
        // Backface culling (client material) relies on every triangle being
        // CCW viewed from outside, i.e. its geometric normal matching the face
        // normal. Check dot(cross(e1, e2), normal) > 0 for both triangles of
        // every quad over a mixed scene that emits all six face classes.
        let mut vol = ChunkVolume::empty();
        vol.set(5, 5, 5, 1);
        for x in 0..8 {
            for z in 0..8 {
                vol.set(x + 10, 3, z + 10, 2);
            }
        }
        let mesh = greedy_mesh(&vol, true);
        for t in 0..mesh.indices.len() / 3 {
            let [a, b, c] =
                [mesh.indices[3 * t], mesh.indices[3 * t + 1], mesh.indices[3 * t + 2]];
            let p = |i: u32| mesh.positions[i as usize];
            let (pa, pb, pc) = (p(a), p(b), p(c));
            let e1 = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
            let e2 = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
            let cross = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let n = mesh.normals[a as usize];
            let dot = cross[0] * n[0] + cross[1] * n[1] + cross[2] * n[2];
            assert!(dot > 0.0, "triangle {t} wound against its normal {n:?}");
        }
    }

    #[test]
    fn flat_top_merges_into_one_quad() {
        // A solid 4x1x4 slab: its +Y top has uniform AO, so it should merge
        // into a single quad.
        let mut vol = ChunkVolume::empty();
        for x in 0..4 {
            for z in 0..4 {
                vol.set(x, 0, z, 1);
            }
        }
        let merged = greedy_mesh(&vol, true);
        // Count +Y-facing quads.
        let top_quads = merged
            .normals
            .iter()
            .step_by(4)
            .filter(|n| n[1] > 0.5)
            .count();
        assert_eq!(top_quads, 1, "flat top with uniform AO should be one quad");
    }
}
