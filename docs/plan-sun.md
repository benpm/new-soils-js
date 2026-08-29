# Semidirectional sunlight — preliminary plan

Status: **design only**, nothing implemented. Written against the light model
as it stands after the RGB rework (`u32` per voxel, `R5 G5 B5 S5`, blocklight
range 31).

## The problem

Skylight today has no direction. `soils_sim::light` propagates one `sky`
channel with a single rule: a level-15 beam travels **straight down** without
loss, every other step loses 1. That is a good model of an overcast sky and a
poor one of a sun. Its symptoms:

* Overhangs and cave mouths are lit the same at dawn as at noon. The sun swings
  through the sky (`day_night` in `main.rs` rotates the `DirectionalLight` and
  `light::update_sky_term` scales the illuminance), but the *grid* it modulates
  never changes shape, so only the brightness moves, never the shadows.
* A wall lights identically on both sides. Nothing in the baked grid knows which
  way the sun is.
* The atmosphere and the terrain disagree: the sky renders a real sun low on the
  horizon while the ground beneath it is lit from directly above.

"Semidirectional" is the target rather than fully directional: keep a diffuse
ambient term so caves and north faces do not go pitch black, and add a
directional term that actually follows the sun.

## Proposed shape

### 1. A second sky channel

The RGB rework leaves 12 bits unused in the light word (`R5 G5 B5 S5` = 20 of
32). Spend 5 of them on a **`sun` channel** alongside the existing ambient
`sky`:

```
bits  0-4    R      blocklight red
      5-9    G      blocklight green
     10-14   B      blocklight blue
     15-19   S      ambient skylight   (today's channel, unchanged rule)
     20-24   D      directional sun    (new)
     25-31   —      spare
```

No extra memory, no change to pool sizing, and the ambient channel keeps
working exactly as it does now — which matters, because it is the fallback
whenever the directional term is unavailable or stale.

### 2. Flood rule for `D`

Same BFS as `S`, with the loss-free axis rotated from "down" to "along the
sun". Because the grid is axis-aligned and the flood is a 6-way BFS, the honest
cheap version is:

* Quantize the sun direction to the **dominant axis pair** — a primary axis
  (the largest component) and a secondary one — and let the beam travel
  loss-free along the primary while losing 1 per step on the secondary. That
  gives a staircase beam that approximates a diagonal without leaving the
  6-way flood.
* At noon the primary is `-Y` and the rule degenerates exactly to today's, so
  the change is a no-op at the time of day everything was tuned against.

Anything better than a staircase (true DDA along an arbitrary vector) means
leaving the BFS, and should not be attempted in the first pass.

### 3. Re-flooding cadence

This is the cost centre and the part most likely to sink the feature. The baked
grid exists precisely because it only changes when voxels change; making it a
function of the time of day means re-flooding **the whole resident set** every
time the sun moves enough to matter.

Plan: quantize the sun to **16 directions over the day cycle** (one step every
112 s at the 30-minute cycle). On each step, enqueue every resident chunk for a
`D`-only re-flood. Budgeted through the existing `LightQueue` /
`plan_light_jobs` machinery so it drains over several seconds rather than
spiking a frame — the sun visibly sweeping into place over a few seconds is
acceptable; a 200 ms hitch every two minutes is not.

Two things make this affordable that would not have been true before:

* **Occlusion culling** already cut the resident set to the visible shell.
* The `D` flood touches only one channel and can skip any chunk whose `S`
  channel is uniformly zero (fully buried) — the same summary the server
  already keeps per chunk.

Measure before committing. If a full re-flood at 16 steps/day is too expensive,
the fallbacks in order of preference are: fewer steps (8), re-flood only chunks
within a shorter radius and let distant terrain keep the ambient term, or
re-flood incrementally (one slab of the resident set per step, accepting that
the sun arrives at different times in different places).

### 4. Shader blend

In `atlas.wgsl`, where today's single term is

```wgsl
let sky_l = params.sky_term * skyf * skyf;
```

become a weighted sum of ambient and directional, with the directional term
gated on the surface actually facing the sun:

```wgsl
let ambient = params.sky_term * SKY_AMBIENT * skyf * skyf;
let facing  = max(dot(n, -params.sun_dir), 0.0);
let direct  = params.sky_term * SKY_DIRECT * sunf * sunf * facing;
let sky_l   = ambient + direct;
```

`SKY_AMBIENT + SKY_DIRECT` should sum to roughly today's 1.0 so existing
exposure tuning survives; something like 0.35/0.65 is the starting guess. The
`facing` term is what makes a wall's two sides differ and costs nothing — the
normal is already in hand.

`sun_dir` joins the terrain uniform next to `sky_term`, written every frame
from the same `WorldTime` the sky uses, so the shading and the rendered sun
cannot drift apart.

## Mirroring

`soils_sim::light` is the oracle and the server's implementation; the WGSL
flood is a mirror of it, and `light_gpu.rs` pins them to each other. The `D`
channel has to land in all three at once or the GPU-vs-`relight_full`
comparison fails immediately — which is the desired behaviour, not a problem.
The sun quantum must be part of the flood's inputs on both sides, not read from
a clock, or the two will disagree by a step at boundaries.

## Phasing

1. Widen the light word's accessors for `D` (no flood yet); prove the format
   change is inert. Property tests and goldens should not move.
2. Implement the `D` flood in `soils_sim::light` against a fixed noon
   direction, assert it reproduces `S` exactly at noon.
3. Mirror it in `light_flood.wgsl`; extend `light_gpu.rs` to compare `D` as
   well, including a low-sun case where the staircase beam is visibly not the
   `S` channel.
4. Shader blend + `sun_dir` uniform. This is the first step with a visible
   result; screenshot it at dawn, noon and dusk.
5. Re-flood scheduling and its budget. Measure the drain time for a full
   resident set at radius 8 before picking the step count.

Steps 1-3 are invisible and safe to land independently. Step 4 changes how
every existing scene looks, so it wants the exposure/tuning pass in the same
change.

## Risks

* **Re-flood cost is the whole gamble.** If a full-set `D` re-flood cannot be
  made to drain inside a couple of seconds at a realistic radius, the feature
  as specified does not fit the baked-grid architecture and the honest answer
  is a shadow map for the sun instead, leaving the L0 grid ambient-only.
* **Staircase artefacts** at low sun angles: a 45° beam through a 6-way flood
  produces visible steps along the shadow edge. Mitigation is the `facing` term
  and the ambient floor softening it; if that is not enough, the fix is more
  sun quanta, not a finer flood.
* **Persisted light.** Chunk light is currently baked and cached; a channel
  that depends on time of day must not be persisted as if it were static, or a
  loaded chunk arrives lit for the wrong hour.
