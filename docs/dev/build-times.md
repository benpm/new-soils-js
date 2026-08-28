# Build times

What makes this workspace slow to build, what was changed on 2026-08-25, and
what each change was actually worth. Read this before adding a dependency, or
before reaching for `debug = 2` and wondering why linking got slow again.

## The measurement

Everything below is the same operation: touch `crates/soils-client/src/main.rs`
and rebuild the client. That is the loop a developer actually feels — the
dependency graph is already built, so what is being measured is compiling one
large crate and linking ~450 rlibs.

| Configuration | Incremental rebuild |
|---|---|
| Baseline (`link.exe`, `debuginfo=2`, Bevy defaults) | **65.3 s** |
| `+ lld-link` | 55.9 s (−14%) |
| `+ trimmed Bevy features, reduced debug info` | **34.2 s (−48%)** |

Clean builds are not measured here — they are dominated by compiling Bevy at
`opt-level = 3`, which is a one-time cost per dependency version and is cached.
The feature trim below is what moves that number; the debug-info change is what
moves the incremental one.

## What was changed

### 1. Link with `lld` instead of `link.exe`

`.cargo/config.toml` points the MSVC target at `lld-link.exe`. Worth 14% on its
own, which was *less* than expected — linking is not the whole story, and the
instinct to blame the linker first was wrong here.

It had a second benefit that does not show up in a timing table: the
intermittent `LNK1104`/`LNK1181` "cannot open file" failures under parallel
test builds stopped. Those cost far more than 14% when they hit, because they
look like corruption and invite a `cargo clean`.

This applies to the MSVC target, which is no longer the default — the
workspace builds under `x86_64-pc-windows-gnullvm` with llvm-mingw, which
links with `lld` already. See [`toolchain.md`](toolchain.md).

### 2. Stop generating debug info nobody reads

```toml
[profile.dev]
debug = "line-tables-only"     # workspace crates
[profile.dev.package."*"]
debug = false                  # dependencies
```

This was the big one. Full `debuginfo=2` on a workspace this size produces
enormous PDBs, and the linker has to process all of it. For scale, at the time
of the change `soils_server.pdb` was **1,135 MB** and `soils_terrainlab.pdb`
**1,248 MB**; the client's, rebuilt under the new settings, is **110 MB**.

`line-tables-only` keeps what panics and backtraces need — file and line — and
drops the variable and type descriptions only a debugger steps through.
Dependencies get nothing at all; a backtrace still *names* Bevy functions from
the symbol table, it just cannot point inside them, which is not where this
project's bugs are.

**If you are about to run a debugger**, set `debug = 2` for the crate you are
stepping through and rebuild it. Do not leave it on.

### 3. Trim Bevy to the features actually used

Bevy 0.19's `default` is `2d + 3d + ui + audio`. This game uses none of the
audio stack, no glTF, no gamepads, no sprites, no animation — verified by
grepping for the types that would appear if it did (`AudioPlayer`, `Gltf`,
`Gamepad`, `Sprite`, `AnimationPlayer`: zero occurrences each).

Both `soils-client` and `soils-terrainlab` now compose Bevy from its
fine-grained features instead. Dropped from the graph: `bevy_audio`, `rodio`,
`lewton`, `ogg`, `cpal`, `dasp_sample`, `bevy_gltf`, `gltf`, `gilrs`,
`bevy_sprite`, `bevy_animation`.

**Trim every workspace member that depends on Bevy, or the trim does nothing.**
Cargo unifies features across members built together, so leaving `default` on
one crate silently restores audio and glTF for the whole `--workspace` build.
`soils-terrainlab` was exactly this trap: the client's trim looked like it
worked under `cargo build -p soils-client` and evaporated under
`cargo test --workspace`. It keeps `2d_bevy_render` because its node canvas
draws through the sprite path.

## What was left alone, and why

* **`opt-level = 3` for dependencies and the hot local crates.** Expensive on a
  clean build, but it is the reason a chunk light flood costs ~300 µs instead of
  ~300 ms in a dev build. The existing comments in the root `Cargo.toml` explain
  which crates need it and what breaks without it. Do not "optimize" this away.
* **`codegen-units`.** The dev default (256) is already tuned for compile speed.
* **Cranelift** and `-Z share-generics`, both of which would help, are nightly.

## If it gets slow again

1. Check whether a new workspace member pulled in Bevy with `default` features.
   That is the failure mode that silently undoes the largest win here.
2. Check whether `debug` got raised for a debugging session and never lowered.
3. `cargo build --timings` produces an HTML report showing which crates dominate
   the wall clock and where the parallelism stalls.
4. If linking specifically regressed, confirm `lld-link.exe` is still on `PATH`
   — cargo falls back to `link.exe` silently.
