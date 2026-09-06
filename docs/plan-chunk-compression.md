# Chunk compression follow-up plan (#20)

**Status:** design only, not implemented.

---

## Goal

Reduce client-side chunk cost by keeping a compressed GPU-friendly representation
longer, then expanding only the voxels the current frame actually needs.

Issue link: [#20](https://github.com/benpm/new-soils-js/issues/20)

---

## Scope

- Keep the existing network chunk codec as-is (palette + LZ4).
- Focus on post-decode representation and mesher/light integration on the client.
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
3. Add acceptance targets for memory and frame time deltas before coding.

---

## Phase B — Occupancy + depth bitfield path

1. Add a compact occupancy/depth bitfield per chunk suitable for:
   - Fast empty/full checks.
   - Conservative occlusion/depth prepass decisions.
2. Feed this representation into chunk-level culling before full mesh expansion.
3. Ensure border-neighbor reads stay conservative to avoid popping at seams.

---

## Phase C — Visible-voxel selective expansion

1. Decompress/materialize voxel data only for chunks that survive culling and are
   scheduled for meshing this frame.
2. Reuse or pool scratch buffers to avoid churn under camera movement.
3. Keep edit paths authoritative: an edited chunk invalidates and rebuilds its
   compressed metadata before reuse.

---

## Phase D — LOD and lighting integration

1. Define how reduced-detail chunks source light data (derived client-side vs
   shipped), aligned with `plan-better-lighting.md`.
2. Ensure LOD transitions do not break chunk-edge continuity for lighting and
   occlusion.
3. Stage rollout: Full LOD first, then Half/Quarter once parity checks pass.

---

## Phase E — Validation and rollout

1. Add regression coverage for:
   - Chunk border visibility continuity.
   - Edit correctness after recompression.
   - Radius-32 residency stability.
2. Re-run performance captures used in PR #19 to confirm real gains.
3. Gate release behind a runtime toggle until default-on confidence is reached.
