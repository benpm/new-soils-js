//! Runtime AssemblyScript → WASM compilation via the `asc` compiler (Node).
//!
//! The compiler is optional: if no `asc` is found, `.ts` scripts are skipped
//! with a log (precompiled `.wasm`/`.wat` still load). Because `asc` spawns
//! Node and can take hundreds of ms, callers run [`compile_ts`] off the ECS
//! tick thread and instantiate the resulting bytes when it returns.

use std::path::Path;
use std::process::Command;

/// How to invoke `asc`. Resolved once and reused.
#[derive(Clone, Debug)]
pub struct Asc {
    /// Program + leading args (e.g. `["asc"]` or `["npx", "--no-install", "asc"]`).
    argv: Vec<String>,
}

impl Asc {
    /// Detect a usable `asc`. Order: `$SOILS_ASC` override → `asc` on PATH →
    /// `npx asc` (locally-installed assemblyscript). Returns `None` if none run.
    pub fn detect() -> Option<Asc> {
        if let Ok(cmd) = std::env::var("SOILS_ASC") {
            let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
            if !argv.is_empty() && probe(&argv) {
                return Some(Asc { argv });
            }
        }
        for argv in candidates() {
            if probe(&argv) {
                return Some(Asc { argv });
            }
        }
        None
    }

    /// Compile `src` (an `.ts` file) to a `.wasm` file at `out`. Returns the
    /// compiled bytes on success, or the captured `asc` diagnostics on failure.
    pub fn compile(&self, src: &Path, out: &Path) -> Result<Vec<u8>, String> {
        let (prog, lead) = self.argv.split_first().expect("non-empty argv");
        let output = Command::new(prog)
            .args(lead)
            .arg(src)
            .arg("--outFile")
            .arg(out)
            .args(["--runtime", "stub", "--optimize", "--use", "abort="])
            .output()
            .map_err(|e| format!("failed to spawn asc: {e}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        std::fs::read(out).map_err(|e| format!("asc produced no output file: {e}"))
    }
}

/// Candidate `asc` invocations, tried in order.
///
/// On Windows, npm installs `asc`/`npx` as `.cmd` shims and `std::process::
/// Command` will **not** resolve a bare `npx` to `npx.cmd` (it only searches for
/// executables, not PATHEXT shims). Without the `.cmd` forms, detection always
/// failed on Windows and every `.ts` script was silently skipped even with
/// assemblyscript installed. Try the shims first there, then the bare names
/// (which is what works on Linux/macOS, and for a real `asc` binary on PATH).
fn candidates() -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = Vec::new();
    if cfg!(windows) {
        v.push(vec!["asc.cmd".to_string()]);
        v.push(vec!["npx.cmd".into(), "--no-install".into(), "asc".into()]);
    }
    v.push(vec!["asc".to_string()]);
    v.push(vec!["npx".into(), "--no-install".into(), "asc".into()]);
    v
}

fn probe(argv: &[String]) -> bool {
    let (prog, lead) = match argv.split_first() {
        Some(x) => x,
        None => return false,
    };
    Command::new(prog)
        .args(lead)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
