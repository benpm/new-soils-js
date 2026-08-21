use super::*;

/// A fixed read view for host-read tests.
struct StubWorld {
    voxel: u8,
    entities: Vec<[f32; 5]>, // [netid, kind, x, y, z]
}
impl ScriptWorld for StubWorld {
    fn voxel(&self, _x: i32, _y: i32, _z: i32) -> u8 {
        self.voxel
    }
    fn entity_count(&self) -> usize {
        self.entities.len()
    }
    fn entity_field(&self, index: usize, field: i32) -> f32 {
        self.entities.get(index).and_then(|e| e.get(field as usize)).copied().unwrap_or(0.0)
    }
}

/// Build a runtime with a single inline WAT module installed (no dir scan / asc).
fn runtime_with(wat: &str) -> ScriptRuntime {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    let engine = Engine::new(&cfg).unwrap();
    let linker = crate::host::build_linker(&engine).unwrap();
    let mut rt = ScriptRuntime {
        engine,
        linker,
        dir: PathBuf::from("."),
        cache: PathBuf::from("."),
        seed: 42,
        asc: None,
        scripts: Vec::new(),
        pending: Vec::new(),
    };
    let module = Module::new(&rt.engine, wat).unwrap();
    rt.install(PathBuf::from("inline.wat"), SystemTime::UNIX_EPOCH, module);
    rt
}

fn empty() -> StubWorld {
    StubWorld { voxel: 0, entities: Vec::new() }
}

#[test]
fn on_tick_edits_and_spawns_once() {
    let wat = r#"(module
      (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
      (import "soils" "spawn" (func $spawn (param i32 f32 f32 f32) (result i32)))
      (global $spawned (mut i32) (i32.const 0))
      (func (export "on_tick") (param i32 f32)
        (call $edit (i32.const 10)(i32.const 70)(i32.const 10)(i32.const 5))
        (if (i32.eqz (global.get $spawned)) (then
          (global.set $spawned (i32.const 1))
          (drop (call $spawn (i32.const 1)(f32.const 8)(f32.const 80)(f32.const 8)))))))"#;
    let mut rt = runtime_with(wat);

    let c0 = rt.run(&empty(), 0, 0.05, &[]);
    assert_eq!(
        c0,
        vec![
            ScriptCommand::Edit { x: 10, y: 70, z: 10, id: 5 },
            ScriptCommand::Spawn { kind: 1, x: 8.0, y: 80.0, z: 8.0 },
        ]
    );

    // Second tick: edit again, no second spawn (guest global persisted).
    let c1 = rt.run(&empty(), 1, 0.05, &[]);
    assert_eq!(c1, vec![ScriptCommand::Edit { x: 10, y: 70, z: 10, id: 5 }]);
}

#[test]
fn get_voxel_reads_live_world() {
    let wat = r#"(module
      (import "soils" "get_voxel" (func $get (param i32 i32 i32) (result i32)))
      (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
      (func (export "on_tick") (param i32 f32)
        (call $edit (i32.const 1)(i32.const 1)(i32.const 1)
          (call $get (i32.const 0)(i32.const 0)(i32.const 0)))))"#;
    let mut rt = runtime_with(wat);
    let world = StubWorld { voxel: 7, entities: Vec::new() };
    let cmds = rt.run(&world, 0, 0.05, &[]);
    assert_eq!(cmds, vec![ScriptCommand::Edit { x: 1, y: 1, z: 1, id: 7 }]);
}

#[test]
fn entity_reads_expose_snapshot() {
    // Spawn at the position read back from entity 0 (proves entity_count/field).
    let wat = r#"(module
      (import "soils" "entity_count" (func $count (result i32)))
      (import "soils" "entity_field" (func $field (param i32 i32) (result f32)))
      (import "soils" "spawn" (func $spawn (param i32 f32 f32 f32) (result i32)))
      (func (export "on_tick") (param i32 f32)
        (if (i32.gt_s (call $count) (i32.const 0)) (then
          (drop (call $spawn (i32.const 1)
            (call $field (i32.const 0)(i32.const 2))
            (call $field (i32.const 0)(i32.const 3))
            (call $field (i32.const 0)(i32.const 4))))))))"#;
    let mut rt = runtime_with(wat);
    let world = StubWorld { voxel: 0, entities: vec![[9.0, 1.0, 3.0, 4.0, 5.0]] };
    let cmds = rt.run(&world, 0, 0.05, &[]);
    assert_eq!(cmds, vec![ScriptCommand::Spawn { kind: 1, x: 3.0, y: 4.0, z: 5.0 }]);
}

#[test]
fn on_edit_reaction_fires() {
    let wat = r#"(module
      (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
      (func (export "on_edit") (param i32 i32 i32 i32 i32 i32)
        (call $edit (local.get 0)(i32.add (local.get 1)(i32.const 1))(local.get 2)(i32.const 3))))"#;
    let mut rt = runtime_with(wat);
    let ev = ScriptEvent::Edit { x: 4, y: 20, z: 6, old: 0, new: 5, by: 1 };
    let cmds = rt.run(&empty(), 0, 0.05, std::slice::from_ref(&ev));
    assert_eq!(cmds, vec![ScriptCommand::Edit { x: 4, y: 21, z: 6, id: 3 }]);
}

#[test]
fn runaway_script_traps_on_fuel_and_is_disabled() {
    let wat = r#"(module
      (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
      (func (export "on_tick") (param i32 f32)
        (call $edit (i32.const 1)(i32.const 1)(i32.const 1)(i32.const 1))
        (loop $l (br $l))))"#;
    let mut rt = runtime_with(wat);
    assert_eq!(rt.script_count(), 1);
    // Traps on fuel exhaustion; partial output (the edit before the loop) is discarded.
    let cmds = rt.run(&empty(), 0, 0.05, &[]);
    assert!(cmds.is_empty(), "trapped script output must be dropped");
    assert_eq!(rt.script_count(), 0, "trapped script is disabled");
    // Stays disabled on subsequent ticks.
    assert!(rt.run(&empty(), 1, 0.05, &[]).is_empty());
}

#[test]
fn rng_is_deterministic_for_seed_and_tick() {
    // Two runtimes, same seed → identical rng-driven spawn coordinate.
    let wat = r#"(module
      (import "soils" "rng" (func $rng (result f32)))
      (import "soils" "spawn" (func $spawn (param i32 f32 f32 f32) (result i32)))
      (func (export "on_tick") (param i32 f32)
        (drop (call $spawn (i32.const 1)(call $rng)(call $rng)(call $rng)))))"#;
    let a = runtime_with(wat).run(&empty(), 3, 0.05, &[]);
    let b = runtime_with(wat).run(&empty(), 3, 0.05, &[]);
    assert_eq!(a, b);
    assert!(matches!(a.as_slice(), [ScriptCommand::Spawn { .. }]));
}
