// Host ABI for server-side scripts. Import from this file:
//   import { onTick, editVoxel, spawn } from "./soils"
// The runtime provides these as the wasm import module "soils"; the scalar
// signatures (i32/f32) mean no string/array marshalling is involved.
//
// Lifecycle exports a script MAY define (all optional):
//   export function on_init(): void
//   export function on_tick(tick: i32, dt: f32): void
//   export function on_edit(x: i32, y: i32, z: i32, old: i32, new_: i32, by: i32): void
//   export function on_player_join(netid: i32): void
//   export function on_player_leave(netid: i32): void

// --- reads (resolve against live world state during the call) ---
@external("soils", "get_voxel")
export declare function getVoxel(x: i32, y: i32, z: i32): i32;
@external("soils", "entity_count")
export declare function entityCount(): i32;
// field: 0=netid, 1=kind, 2=x, 3=y, 4=z
@external("soils", "entity_field")
export declare function entityField(index: i32, field: i32): f32;
@external("soils", "seed")
export declare function seed(): i32;
@external("soils", "tick")
export declare function currentTick(): i32;
@external("soils", "rng")
export declare function rng(): f32; // deterministic uniform [0,1)

// --- writes (buffered, applied by the server after the call) ---
@external("soils", "edit_voxel")
export declare function editVoxel(x: i32, y: i32, z: i32, id: i32): void;
@external("soils", "spawn")
export declare function spawn(kind: i32, x: f32, y: f32, z: f32): i32;
@external("soils", "despawn")
export declare function despawn(netid: i32): void;
@external("soils", "set_velocity")
export declare function setVelocity(netid: i32, x: f32, y: f32, z: f32): void;
@external("soils", "set_pos")
export declare function setPos(netid: i32, x: f32, y: f32, z: f32): void;
@external("soils", "log")
export declare function log(level: i32, code: i32): void;

// Kind ids match entities.yaml declaration order.
export const KIND_PLAYER: i32 = 0;
export const KIND_CRITTER: i32 = 1;
