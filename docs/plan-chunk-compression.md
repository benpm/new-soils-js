# Chunk compression follow-up plan (#20)

**Status:** design only, not implemented.

---

## Goal

Reduce client-side chunk cost by meshing from compressed occupancy data and
deferring material fetch to only visible surfaces.

Issue link: [#20](https://github.com/benpm/new-soils-js/issues/20)

---

## Core scheme to apply

1. Split chunk voxel storage into:
   - occupancy: 1 bit/voxel
   - material: 8-bit material id
2. Build surface topology from occupancy bits directly (no full material
   decompression in the meshing step).
3. Attach source voxel/cell identity to generated surface primitives.
4. Run a depth + integer ID pass to determine the visible surface IDs.
5. Resolve materials in a deferred pass by decoding IDs and fetching only the
   required material bytes.

Optional extension:

- Use the ID/depth visibility result to drive a GPU-side material-brick cache
  (virtual-texture style residency).

---

## Scope

- Keep the existing network chunk codec as-is (palette + LZ4).
- Focus on post-decode representation and mesher/render integration on the client.
- Preserve chunk visual parity and edit correctness.

Out of scope for this plan:

- Protocol redesign.
- New gameplay-facing chunk formats.

---

## Phase A — Baseline and invariants

1. Record current client CPU/GPU timings for chunk apply, meshing, and light work
   at radius 8 and radius 32.
2. Pin invariants that must not change:
   - `voxel_at` behavior for unloaded/missing chunks.
   - Correct face visibility at chunk borders.
   - Light queue and flood semantics.
3. Add acceptance targets for memory, meshing bandwidth, and frame time deltas
   before coding.

---

## Phase B — Occupancy representation

1. Add occupancy storage at 1 bit/voxel, packed by X-row into `u32` lanes
   (`32` voxels per row for `32^3` bricks).
2. Keep material bytes in a separate storage path keyed identically to occupancy.
3. Build fast occupancy helpers for:
   - empty/full row tests
   - active-cell detection
   - border-neighbor conservative checks at chunk seams

---

## Phase C — Occupancy-driven surface construction

1. Update GPU meshing to consume occupancy bits directly rather than full 8-bit
   voxel material arrays.
2. Generate surface output with a packed source ID per primitive/vertex that
   encodes:
   - brick identity
   - local source voxel/cell coordinates (`x,y,z` in 5 bits each for `32^3`)
3. Preserve existing topology and seam correctness versus current mesher output.

---

## Phase D — Depth+ID visibility pass and deferred materials

1. Add a depth + integer ID raster pass for generated terrain surfaces without
   material sampling.
2. Add a deferred material resolve pass:
   - read visible surface ID per pixel
   - decode brick + local voxel/cell id
   - fetch material id only for visible fragments
3. Integrate with current shading path so color/atlas lookup remains equivalent.

---

## Phase E — Residency, LOD, and lighting interaction

1. Use ID-pass visibility to identify visible bricks and drive optional
   on-demand GPU material-brick uploads.
2. Align material-brick residency and LOD transitions so downgraded chunks do
   not break lighting/occlusion continuity.
3. Keep decisions coordinated with `plan-better-lighting.md` where downsampled
   geometry requires downsampled light treatment.

---

## Phase F — Validation and rollout

1. Add regression coverage for:
   - Chunk border visibility continuity.
   - Edit correctness after occupancy/material recompression updates.
   - Deferred material resolve parity (same visible materials as baseline path).
   - Radius-32 residency stability.
2. Re-run performance captures used in PR #19 and report:
   - meshing-stage bandwidth reduction
   - frame-time impact during heavy chunk churn
3. Gate release behind a runtime toggle until default-on confidence is reached.
