//! Exercises the real AssemblyScript → WASM compile path. Auto-skips when no
//! `asc` toolchain is available (mirrors the GPU oracle tests' adapter skip),
//! so CI without Node still passes; set `SOILS_ASC` to force a specific command.

use soils_script::{Asc, ScriptRuntime};
use std::time::{Duration, Instant};

#[test]
fn compiles_and_loads_assemblyscript_when_asc_present() {
    if Asc::detect().is_none() {
        eprintln!("skipping asc pipeline test: no `asc` found (set SOILS_ASC or install assemblyscript)");
        return;
    }

    let dir = std::env::temp_dir().join(format!("soils-asc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mod.ts"),
        r#"
@external("soils", "edit_voxel")
declare function editVoxel(x: i32, y: i32, z: i32, id: i32): void;
export function on_tick(tick: i32, dt: f32): void {
  editVoxel(1, 2, 3, 4);
}
"#,
    )
    .unwrap();

    let mut rt = ScriptRuntime::new(&dir, 1).expect("runtime");
    // Compilation runs on a background thread; poll until it lands.
    let start = Instant::now();
    while rt.script_count() == 0 && start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
        rt.poll();
    }
    assert!(rt.script_count() >= 1, "asc-compiled .ts script should load");

    let _ = std::fs::remove_dir_all(&dir);
}
