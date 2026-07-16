// Example server script. Demonstrates the full downstream-event loop:
//   on_tick  → autonomous world mutation (grows a pillar, spawns a critter)
//   on_edit  → reacts to a player edit by capping it with stone
//
// Edit this file while the server runs (SOILS_SCRIPTS=1) to see hot-reload.
import * as soils from "./soils";
import { KIND_CRITTER } from "./soils";

// Block ids (see blocks.yaml): 0=air, 3=stone (adjust to taste).
const STONE: i32 = 3;

let spawned: bool = false;

export function on_tick(tick: i32, dt: f32): void {
  // Grow a slowly rising pillar at (10, y, 10): one block every 20 ticks (~1s).
  if (tick % 20 == 0) {
    const y = 64 + (tick / 20) % 32;
    soils.editVoxel(10, y, 10, STONE);
  }
  // Spawn one ambient critter on the first tick.
  if (!spawned) {
    spawned = true;
    soils.spawn(KIND_CRITTER, 8.0, 80.0, 8.0);
  }
}

// Cap every player-placed block with a stone block directly above it.
export function on_edit(x: i32, y: i32, z: i32, old: i32, new_: i32, by: i32): void {
  if (new_ != 0) {
    soils.editVoxel(x, y + 1, z, STONE);
  }
}
