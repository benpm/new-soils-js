# Plan: Better Lighting (Cached Voxel Radiance Cascades)

## Idea

Instead of computing all the lighting client-side, coarse (per-voxel) lighting should be stored with the world data and cached on the client. The coarse lighting should be saved alongside the voxels as chunk data, and the fine lighting should be computed client-side only entirely on the GPU, never being copied to CPU or stored. 

- Use the implementation in `~/projects/voxel_radiance_cascades` as a starting point 
- Make sure to add notes to this plan that would make it no longer necessary to be able to access that implementation in its entirety
- Cache and store the coarse lighting information. 
- Use bilinear interpolation or better to interpolate the light values across voxels to acheive a smoother lighting effect
- If a chunk is likely to be culled from drawing entirely (surrounded on all sides by other chunks that have a high density of voxels (measure this upon chunk gen using an atomic counter), prioritize its lighting to be done later
- Only compute higher detailed lighting, dynamic light, etc, for chunks that are near the player, and ONLY ON CLIENT

## Detailed Plan

*Written 2026-08-29. Status: **planned, not started**. The last bullet of §3 is
the one thing here that cannot be resolved without going back to Shadertoy.*

### 0. What already exists

This plan is mostly not new machinery. Three of its five bullets describe
things the codebase already does, and writing it against a blank slate would
have produced a second lighting system beside the working one. So, precisely:

**Coarse light (L0)** — `crates/soils-sim/src/light.rs`. One byte per voxel:
skylight in the high nibble, blocklight in the low. `ChunkLight` is
`Uniform(u8) | Dense(Box<[u8]>)`, which matters more than it looks — open-sky
air and enclosed rock are both uniform, so most chunks cost one byte and the
flood skips them entirely. `relight_full` is the from-scratch oracle;
`light_new_chunk` and `apply_voxel_change` are the incremental paths, property-
tested to agree with it. The server runs the same flood (phase 9, 4 ms/tick
budget, edits relight inline).

**Fine light (radiance cascades)** — `radiance.wgsl`, `gi.rs`, `gi_blit.wgsl`,
`gi_irradiance.wgsl`, opt-in behind `SOILS_GI=1`. Phase 12 already moved the
occupancy fill onto the GPU (`gi_blit` blits the mesher's resident voxel and
padded-light buffers into the volumes; the 262 KB/30-frame CPU rebuild is
gone), gated top-cascade escapes on baked skylight, and projected per-probe
ambient-cube irradiance once per cycle. Pinned by GPU-vs-CPU oracle tests in
`tests/gi_gpu.rs`.

So bullets 2 (fine lighting is GPU-only and never copied back) and 5 (it only
runs near the player, client-side) are **already true**. What is left is
bullets 1, 3 and 4 — and bullet 1 is the one that pays.

### 1. The problem worth solving: the client re-floods every chunk it receives

Light is *derived* state on both sides today. The server rebuilds it on chunk
residency; phase 9 recorded that decision explicitly ("light persistence
skipped — derived, rebuilt on residency"). The client does the same: every
chunk that arrives is pushed onto `LightQueue` (`demand.rs:234`) and flooded
locally.

That is the measured cost. The client's frame is *light*-bound, not draw-bound:
steady state sits around 116 fps, but a fresh join spends roughly four to five
minutes at ~46 fps while the light queue drains. Nothing about that work is
client-specific — the server computed the identical answer moments earlier and
threw it away.

**So bullet 1 is not a caching micro-optimisation. It is deleting a four-minute
join penalty by shipping an answer that already exists.**

That reframing also settles a question the bullet leaves open — whether
"stored with the world data" means the region file or the wire. It means both,
and for different reasons:

* **On the wire** is what removes the client-side flood. This is the win.
* **In the region file** is what removes the *server-side* flood on residency.
  Smaller, and strictly optional — the server can always recompute. Worth doing
  second, and only if the residency flood shows up in a tick profile.

### 2. Phases

Each is shippable and test-gated on its own. The order is deliberate: phase A
is the one with the measurable payoff, and phase C is the one that can be cut
without touching the others.

---

**Phase A — light on the wire.**

Extend the chunk payload so a chunk arrives with its light. `ChunkLight` is
already `Uniform | Dense`, so the encoding is nearly free for the common case:
one discriminant byte plus one value for a uniform chunk, and `CHUNK_CUBED`
bytes for a dense one, LZ4'd alongside the voxels the way `chunk_codec.rs`
already handles palettes.

The client stops queueing arrived chunks for flooding and adopts the shipped
grid. `LightQueue` does *not* disappear — it still serves local edits and the
`reconcile_sky_below` correction — but its steady-state input becomes edits
rather than every chunk in the subscription.

The subtlety, and the reason this is not a one-day change: the client's flood
is *optimistic about the sky*. `open_sky_above` answers "chunk above not
loaded" with `true`, and `reconcile_sky_below` corrects it when the chunk above
arrives. The server has no such ambiguity — it knows what is resident. So a
shipped grid and a locally-flooded grid are not always the same grid, and the
client must not "correct" an authoritative one with its optimistic guess.
Decide explicitly whether shipped light is final (and `reconcile_sky_below`
becomes edit-only) or advisory.

*Gate:* a scenario asserting a joined client's light grid is byte-identical to
the server's for every chunk in the subscription, plus the existing
`relight_full` oracle still agreeing after an edit storm. And a measurement —
this phase justifies itself with a number or not at all: join-to-steady-state
fps, before and after.

*Risk:* payload size. Chunk streaming v2 got the join burst from 23 MB to
498 KB and there is a 2 MB regression gate. A dense light grid is
`CHUNK_CUBED` bytes before compression, so measure against that gate early —
if uniform chunks dominate as expected the cost is trivial, and if they do not,
that is worth knowing before the encoder is written.

---

**Phase B — smooth interpolation.**

Bullet 3. Light is currently sampled per-voxel, so a lit surface changes in
visible steps. Trilinear (not bilinear — this is a 3D grid, and the bullet's
"bilinear" is a slip worth naming) interpolation across the eight surrounding
voxel centres removes the banding.

The complication is that interpolating *through solid geometry* leaks light
around corners: a lit voxel adjacent to a wall bleeds its value into the dark
side. The standard fix is to weight each of the eight samples by the occupancy
of its cell and renormalise, discarding solid neighbours. The mesher already
has the padded light volumes this needs — `gi_blit.wgsl` blits them — so the
data is in place and this is a shader change, not a pipeline change.

*Gate:* the existing GPU-vs-CPU oracle extended with an interpolation case, and
specifically a wall case: a probe one voxel inside solid geometry must not
brighten because its neighbour across the wall is lit.

---

**Phase C — deprioritise light for chunks that cannot be seen.**

Bullet 4, and the one to cut first if the budget runs out. Note what already
exists: occlusion culling withholds *sealed* chunks — every neighbour presents
a solid layer toward it — and that is 26% of a radius-8 subscription
(`World::sealed`). Sealed is a stronger and cheaper predicate than the bullet's
"surrounded by neighbours with a high density of voxels", and it is already
computed.

So the honest version of this phase is: **reuse `sealed` as the deprioritisation
key before writing a density counter.** A density counter is a second, fuzzier
predicate for the same purpose, and the atomic-counter-at-gen-time framing
prescribes an implementation for a number the mesher may already be able to
produce. Write the density counter only if `sealed` proves too conservative in
a profile.

*Gate:* light-queue drain ordering under a fresh join — visible chunks must
complete before withheld ones — and no change to the final grid, only to when
each chunk reaches it.

---

**Phase D (optional) — light in the region file.**

Only if the server's residency flood shows up in a tick profile. The machinery
is now trivially available: `store.rs`'s `Store<V>` and `paged.rs` were
extracted from the chunk cache precisely so a second per-chunk structure could
reuse them, and block data (chests) is the existing second tenant. Light would
be a third, at the same slot index, with a `Codec` impl over `ChunkLight`.

The one design note: unlike chest contents, light is *derived*, so its
`keep` predicate during compaction should discard rather than preserve — a
pruned light page costs a re-flood, not lost data. That is the opposite of
block data's predicate and is exactly why `paged::compact` takes the predicate
from the caller.

### 3. The reference implementation, and what is actually in it

The idea section says to use `~/projects/voxel_radiance_cascades` as a starting
point and to write down enough here that the plan no longer depends on it.
Doing that turned up two things that change what this section can promise.

**First: most of that repo is not there.** Its own `CLAUDE.md` warns of a known
broken state, and the warning is accurate — verified 2026-08-29:

```
$ md5sum ~/projects/voxel_radiance_cascades/shaders/*
204e90ed12408b3fd9e172904c097834  buffer_A.glsl
204e90ed12408b3fd9e172904c097834  buffer_B.glsl
204e90ed12408b3fd9e172904c097834  buffer_C.glsl
204e90ed12408b3fd9e172904c097834  common.glsl
204e90ed12408b3fd9e172904c097834  cubemap_A.glsl
204e90ed12408b3fd9e172904c097834  fragment_final.glsl
```

All six files are byte-identical copies of `common.glsl`. The cascade passes
themselves — how cascades are allocated across the three buffers, the interval
scheme, the merge, the sky cubemap, the final gather — **are not on disk** and
must be re-fetched from <https://www.shadertoy.com/view/M3ycWt> if they are
wanted. This plan therefore cannot make that repo unnecessary, and claiming
otherwise would be the more expensive kind of wrong.

**Second, and more usefully: we do not need them.** That repo is a *second*
radiance-cascades implementation. This one already has a working, test-gated
one, and the two differ in the choice that matters most:

| | reference (Shadertoy) | this repo |
|---|---|---|
| direction encoding | concentric square rings in a `probeSize²` tile | octahedral |
| host | Shadertoy multipass, RGBA32F feedback buffers | wgpu compute + storage volumes |
| oracle | none (visual) | CPU oracle, `tests/gi_gpu.rs` |

Adopting the ring encoding would mean rewriting `radiance.wgsl` and every test
that pins it, to gain nothing this plan asks for — none of the five bullets is
about direction encoding. **The reference is worth reading for ideas, not
porting.**

What survives on disk, and is genuinely worth knowing:

* **The probe layout.** A probe is a `probeSize × probeSize` tile of texels
  encoding a hemisphere, laid out as concentric square rings: ring index
  `thetai` is the Chebyshev distance from the tile centre and gives the polar
  angle; position along the ring gives azimuth; ring `k` holds `4 + 8k`
  directions. Compared to octahedral this trades a little direction uniformity
  for very cheap ring indexing.
* **`ComputeDir(uv, probeSize)` / `ProjectDir(dir, probeSize)`** are exact
  inverses up to ring quantisation, and *must stay so or merging breaks* —
  an edit to one requires the matching edit to the other. `ProjectDir` returns
  `vec2(-1)` for `dir.z <= 0` (below the hemisphere). `ComputeDir` takes a
  hardcoded 4-tap path for `probeSize <= 4.5`, and that small-probe branch has
  no counterpart in `ProjectDir`.
* **Probe-local space.** Directions come back in probe-local coordinates with
  `+Z` as the hemisphere axis, so they compose with `TBN(N)`.
* Utility shapes worth copying verbatim if a similar helper is ever needed:
  `ABox` returns `vec2(tNear, tFar)` and signals a miss by `tNear > tFar` with
  no bool flag; `DFBox` takes a box with its *corner* at the origin, not a
  centred box; `BRDF_GGX` returns `vec3(0.)` on NaN rather than clamping its
  inputs.

That is the whole of the usable content. Nothing in phases A–D depends on the
missing passes.

### 4. Relationship to the other lighting plans

Three lighting documents now exist and they are not alternatives:

* **This one** — where coarse light is *computed and stored*. Changes the
  pipeline, not the format.
* [plan-rgb-light-rework.md](plan-rgb-light-rework.md) — RGB blocklight at
  range 31, one `u32` per voxel (`R5 G5 B5 S5`) replacing the packed byte.
  Changes the format, not the pipeline.
* [plan-sun.md](plan-sun.md) — semidirectional sunlight, a second 5-bit channel
  in the light word's spare bits.

**Sequencing matters here.** Phase A puts the light grid on the wire, and the
RGB rework quadruples the size of every entry in it. Doing A first means
encoding a byte grid and then re-encoding a `u32` grid; doing the RGB rework
first means one encoder, written once, against the final format — at the cost
of delaying the measured win.

The recommendation is **RGB first, then phase A**, on the grounds that the
payload-size risk in phase A is the thing most likely to sink it, and it should
be measured against the format that will actually ship rather than one that is
already scheduled for replacement. `plan-rgb-light-rework.md` also notes it
pays for itself by halving `N_SLOTS`, which interacts with the "cull all-air
chunks" item in `Tasks.md` — that is where the headroom comes from.
