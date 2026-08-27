# Plan: RGB blocklight, doubled range

Status: **designed, not landed.** A partial implementation of step 1 sits in a
git stash (`wip: rgb light format`); treat it as a sketch, not a starting
point, and follow the phasing below.

Related: [plan-better-lighting.md](plan-better-lighting.md) sets the direction
this has to be compatible with — coarse per-voxel light stored *with* the world
data rather than recomputed by every client, fine light GPU-only. This plan
changes what one coarse light value **is**; that one changes **who computes and
ships it**. They are orthogonal and this one should land first, because the
format is what the other has to persist and put on the wire.

## Why

Two asks that turn out to be the same change:

* **Coloured light blocks.** `blocks.yaml` already carries per-block RGB
  `emission` (linear radiance) and the radiance-cascades voxelizer already uses
  it. The L0 grid throws it away: `BlockRegistry::light_table` collapses each
  block to one 0-15 scalar, and `atlas.wgsl` re-tints it with a single hardcoded
  warm colour. Two differently-coloured lamps overlapping cannot look right,
  because the grid only remembers one intensity and never which colour put it
  there.
* **Doubled range.** Range *is* level: blocklight loses 1 per voxel step, so a
  4-bit channel reaches 15 voxels and nothing else. Reaching 31 needs a fifth
  bit. There is no way to buy range inside the existing byte.

## The format

One `u32` per voxel, replacing the packed byte:

```
bits  0-4    R      blocklight red     0-31
      5-9    G      blocklight green   0-31
     10-14   B      blocklight blue    0-31
     15-19   S      skylight           0-31
     20-31   —      spare
```

`MAX_LIGHT` goes 15 → 31. Attenuation stays 1 per step, so range doubles as a
consequence of the wider channel rather than by changing the flood's rule — the
sky beam's loss-free fall and every other propagation rule are untouched.

The 12 spare bits are not slack for its own sake: the directional sun channel
in [plan-sun.md](plan-sun.md) is meant to live at bits 20-24.

### Memory, and why the pool halves

The client's light pool is the only place this hurts: `N_SLOTS` × 32 KB goes to
`N_SLOTS` × 128 KB. At today's 6144 slots that is 192 MB → 768 MB of VRAM,
which is not acceptable alongside a large draw distance.

`N_SLOTS` therefore drops to **3072** (384 MB). That is only survivable because
occlusion culling landed first: at radius 8 the server withholds ~26% of a 4913
chunk subscription, and sealed chunks never take a slot. Verify with the
draw-distance test before and after — if resident counts at radius 8 come close
to 3072, the answer is a better cull (all-air chunks are the obvious next win),
not a bigger pool.

`ChunkLight::Uniform` carries most of the cost anyway: open-sky air and
enclosed rock are one word each, and only diverged chunks promote to `Dense`.

## What has to change, in dependency order

### 1. `soils-sim/src/light.rs` — the oracle

Everything else mirrors this, so it lands alone and first.

* `pub type Packed = u32`, accessors `red`/`green`/`blue`/`sky`/`rgb`/`pack`.
  Keep a scalar `block(packed) -> u8` returning `max(r, g, b)`: chunk
  summaries, `darkest_walkable_near` and most tests only ask "is there
  blocklight here", never which colour, and keeping it spares them all.
* `Channel` gains `Red`/`Green`/`Blue` in place of `Block`, with a
  `COLOUR_CHANNELS` array. `get`/`set` become a shift and a mask rather than a
  match, which is also how the WGSL mirror will do it.
* `LightWorld::emission` returns `[u8; 3]`.
* The floods run **once per colour channel over a shared seed queue**.
  `propagate` already skips a seed whose level is 0 in the channel it is
  running, so a red-only lamp costs a queue walk in the green and blue passes
  and nothing more. This keeps `relight_full`, `light_new_chunk` and
  `apply_voxel_change` structurally identical instead of introducing a
  three-wide flood.
* `ChunkLight` stores `Packed`; `as_dense_bytes`/`as_bytes_mut`/
  `from_bytes_collapsed` become the `_words` forms. Note this is
  `soils_sim::light::ChunkLight`, not `ChunkVolume` — the many `as_bytes_mut`
  calls in `chunk_codec.rs` and `region.rs` are voxel ids and must not be
  touched.

Gate: the existing property tests (incremental paths vs `relight_full`) pass
unchanged with every emitter grey, i.e. `[n, n, n]`. That is the proof the
widening is inert before any colour exists.

### 2. `soils-worldgen` — where colour comes from

`light_table()` returns `Vec<[u8; 3]>` instead of `Vec<u8>`, mapping each
channel of `emission` through the same curve the scalar version used, scaled so
the brightest entry lands on 31 rather than 15.

Then author the lamps. Today there are exactly two emissive blocks (Diamond Ore
and Ruby Ore), which is not enough to see colour blending at all. Add a set of
lamp blocks spanning the hue circle — red, orange, yellow, green, cyan, blue,
violet, white — with matching atlas tiles.

### 3. `soils-server` — summaries and the flood host

Mostly mechanical: `light_levels: Vec<u8>` → `Vec<[u8; 3]>`, `WorldLight`
follows the trait, and `LightSummary` keeps working untouched because it goes
through the scalar `block()`.

### 4. Client pool and GPU flood

* `pool.rs`: `N_SLOTS` 6144 → 3072; the light pool is indexed one word per
  voxel instead of four voxels per word. This is the fiddliest edit in the
  whole change — `light_flood.wgsl` currently packs 4 cells into a `u32` and
  every index in `reseed`/`beam`/`relax` reflects that. One voxel per word is
  simpler than what is there now; resist the urge to keep the packing.
* The emitters table becomes RGB rows, and the point-light rows
  (`GpuPointLight`, the player's own light) gain a colour.
* `atlas.wgsl`: the blocklight term becomes the grid's own RGB rather than a
  scalar times `BLOCK_LIGHT`. The fixed warm tint disappears — which means
  every existing scene's blocklight changes colour, so re-check the exposure
  tuning in the same change.

### 5. Tests and demo

* `light_gpu.rs` compares the GPU flood against `relight_full` per channel. Add
  a case with two differently-coloured emitters whose ranges overlap: the
  overlap must carry both channels, which is the single assertion that would
  have failed under every cheaper design considered.
* A demo scene in the shape of `gi_demo.rs` (`SOILS_LIGHT_DEMO=1`): a dark room
  with one lamp of each colour in a row, far enough apart to read individually
  and close enough that neighbours' pools overlap. This is the artefact that
  makes "coloured light works" checkable by looking.

## Risks

* **Pool sizing is the real risk.** 3072 slots is a bet on the cull. Measure
  peak resident chunks at radius 8 first; if it is over ~2500 this plan needs
  the all-air cull before it can land.
* **Range 31 doubles the flood's reach**, so a single edit dirties a larger
  neighbourhood and `apply_voxel_change` does more work per broken block.
  `process_light` was historically the client's frame bottleneck; re-measure
  it, and do not assume the GPU flood absorbs it silently.
* **Everything looks different.** Removing the hardcoded warm tint changes
  every lit scene and every recorded demo. Land the exposure re-tune with it,
  not after.
* **Persistence.** Nothing persists L0 light today, which is the only reason
  this is a pure in-memory format change. The moment
  [plan-better-lighting.md](plan-better-lighting.md) lands, the format becomes
  a **wire and disk** format, and changing it again means a version bump and a
  migration. Get it right now.
