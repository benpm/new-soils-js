# Documentation

Mostly agent-maintained documentation of the project. Start with
`architecture.md` for how things work now, `roadmap.md` for direction, and
`tasks.md` for what is left.
Setting up a machine to build this repo: [`dev/toolchain.md`](dev/toolchain.md).

## Current state

| File | What it is |
|---|---|
| [`architecture.md`](architecture.md) | How every system works today: protocol, transports, server tick, chunk lifecycle, inventory, rendering, GI, prediction, SpacetimeDB mirror, testing. The first thing to read. |
| [`roadmap.md`](roadmap.md) | High-level product and technical direction, including completed foundations and deferred upgrades. |
| [`tasks.md`](tasks.md) | Individual actionable work, ordered from current priorities to staged ideas. |
| [`perf-report.md`](perf-report.md) | The optimization narrative with measurements, the methodology, and the ranked list of what to optimize next. |
| [`dev/`](dev/) | Working notes for people (and agents) touching the code — see below. |

## Design

| File | What it is |
|---|---|
| [`concepts.md`](concepts.md) | Design philosophy and mechanics: what this is trying to be, and where it deliberately departs from Minecraft. |
| [`conceptual_design.md`](conceptual_design.md) | Story and setting. |
| [`future.md`](future.md) | Loose ideas, explicitly not ready for implementation. |

## Plans

Each plan states its own status at the top. They are historical once
implemented — `architecture.md` describes what actually shipped.

| File | What it is |
|---|---|
| [`analysis.md`](analysis.md) | The original analysis the rewrite was planned from. |
| [`plan-game-systems.md`](plan-game-systems.md) | Entities, authoritative server, delta replication. **Implemented.** |
| [`plan-rendering.md`](plan-rendering.md) | Renderer, lighting, GI. **Implemented.** |
| [`plan-ui.md`](plan-ui.md) | Inventory, the item ring, and the `UiMode` refactor. **Phases 0–5 shipped**; remainder in [`tasks.md`](tasks.md). |
| [`plan-rgb-light-rework.md`](plan-rgb-light-rework.md) | RGB blocklight in a `u32` (5:5:5:5) with range 31, and the pool halving that pays for it. **Designed, not landed.** |
| [`plan-sun.md`](plan-sun.md) | Semidirectional sunlight: a directional channel in the light word's spare bits. **Design only.** |
| [`plan-better-lighting.md`](plan-better-lighting.md) | Coarse light stored with world data, fine light GPU-only. **Sketch.** |
| [`plan-storage.md`](plan-storage.md) | Region files, chunk persistence, and containers. **Phase 1 shipped.** |
| [`plan-death-chest.md`](plan-death-chest.md) | A minimal death model, the grave it drops, and filming it. **Plan only.** |
| [`plan-server-commands.md`](plan-server-commands.md) | Player-typed slash commands: server-parsed lines, account roles, and the chunk-granular bulk-edit engine behind `/fill` and the excavator. **Design only.** |

## Working notes (`dev/`)

| File | What it is |
|---|---|
| [`dev/toolchain.md`](dev/toolchain.md) | Building on Windows with LLVM/mingw-w64 (`gnullvm`) instead of MSVC: the sysroot, the cc-rs trap, and static CRT linking. |
| [`dev/debug.md`](dev/debug.md) | Symptom-first debugging field guide: the traps this codebase actually produced, and how to tell them apart quickly. Read this before a long debugging session. |
| [`dev/server-tick.md`](dev/server-tick.md) | Tick phase ordering, the determinism rules, and why login runs off the tick thread. |
| [`dev/dashboard.md`](dev/dashboard.md) | The public dashboard of recorded test videos, and how it is deployed. |

## Assets

`screenshots/` and `media/` hold the images and clips referenced from
[`../README.md`](../README.md). The GI demo pair must be retaken at matched
framing (camera ≈ `272, 278, 261`) to stay comparable.
