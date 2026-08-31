# Build toolchain

How this workspace builds on Windows without Visual Studio, the Windows SDK, or
any MSVC library. Read this if a fresh checkout fails to link, or if you are
setting up a new machine.

## The short version

Two things are required:

1. The **`stable-x86_64-pc-windows-gnullvm`** rustup toolchain, set as default.
2. **llvm-mingw** extracted to `C:\llvm-mingw`, which `.cargo/config.toml`
   points at by absolute path.

Nothing needs to go on `PATH`, and the binaries this produces have no
dependency on llvm-mingw at run time.

```sh
rustup toolchain install stable-x86_64-pc-windows-gnullvm
rustup default stable-x86_64-pc-windows-gnullvm
# then download the ucrt-x86_64 zip from
# https://github.com/mstorsjo/llvm-mingw/releases and extract it to C:\llvm-mingw
cargo build --workspace
```

If your llvm-mingw lives somewhere else, either edit the four paths in
`.cargo/config.toml` or delete that block and put its `bin` directory on `PATH`
— cargo and `cc-rs` both find the tools there on their own.

## Why gnullvm and not msvc

`x86_64-pc-windows-msvc` links against the MSVC CRT with `link.exe` and needs a
Visual Studio installation plus the Windows SDK. `x86_64-pc-windows-gnullvm` is
the other supported Windows target: same UCRT, but the mingw-w64 flavour of the
import libraries and startup objects, linked with `lld`. Nothing in the chain
is Microsoft's. It is not the mingw *GCC* target either — `gnullvm` is
clang/lld end to end, which is why it does not want a GCC install.

The MSVC target still works if you have the tooling, and `.cargo/config.toml`
keeps its `lld-link` setting for that case (see
[`build-times.md`](build-times.md)). It is simply not the default any more.

## Why stock LLVM is not enough

A normal `C:\Program Files\LLVM` install has `clang`, `lld` and `lld-link`, and
it is tempting to assume that is the whole story. It is not: what is missing is
the *sysroot* — the mingw-w64 import libraries, C headers, and compiler
runtime. Clang is a cross-compiler with no libraries of its own.

rustup does ship a small self-contained sysroot with the gnullvm target, at
`lib/rustlib/x86_64-pc-windows-gnullvm/lib/self-contained`, but it holds only
the CRT startup objects and seven import libraries (`kernel32`, `user32`,
`ws2_32`, `ntdll`, `userenv`, `dbghelp`, `msvcrt`). That is a hello-world's
worth. Pointing the target's linker at a stock `clang` and building anything
real fails immediately:

```
lld: error: unable to find library -lgcc
lld: error: unable to find library -lgcc_eh
lld: error: unable to find library -lmoldname
lld: error: unable to find library -ladvapi32
lld: error: unable to find library -lshell32
```

Those come from llvm-mingw, which bundles LLVM together with a built
mingw-w64 and the `x86_64-w64-mingw32-clang` driver that knows where to find
it. Installing it is the entire fix — with its `bin` on `PATH` and no other
change, `cargo build --workspace` goes green.

## The `cc-rs` trap

Setting only the linker is not sufficient, and the failure is easy to
misdiagnose because it arrives from a build script rather than from linking.
`cc-rs` resolves the C compiler from `PATH`, independently of the `linker`
key, so it finds a stock `clang.exe` — which compiles for a mingw target it has
no headers for:

```
ring-0.17.14/include/ring-core/check.h:27:11:
  fatal error: 'assert.h' file not found
```

Hence the `[env]` block in `.cargo/config.toml` setting
`CC_x86_64_pc_windows_gnullvm`, `CXX_…` and `AR_…` to llvm-mingw's tools.
`ring` is the crate that surfaces this, but any `-sys` dependency with C in it
would.

Putting llvm-mingw on `PATH` instead of using the `[env]` block solves this too,
for the same reason it solves the linker: `cc-rs` then finds the right compiler
first.

## The exe that builds but will not start

Getting a green build is not the end of it. The first working configuration
produced binaries that linked fine and then died instantly with no output:

```
0xC0000135   # STATUS_DLL_NOT_FOUND
```

The missing DLL is llvm-mingw's own `libunwind.dll`. rustc emits `-lunwind` in
the *dynamic* bracket of the link line, and lld's mingw search order prefers
`libunwind.dll.a` over `libunwind.a` — so the unwinder gets linked as an import
and the exe needs a DLL that only exists inside `C:\llvm-mingw\bin`. It works
on the machine that built it *if* that directory happens to be on `PATH`, and
nowhere else. A five-line `fn main` reproduces it, so this is the toolchain's
behaviour rather than anything about this workspace.

The fix is `-C target-feature=+crt-static` in the target's `rustflags`, which
resolves the unwinder against the static archive. Confirmed by dumping the
import table:

```sh
llvm-readobj --coff-imports target/debug/soils-server.exe
```

Before: `libunwind.dll` present. After: gone, and the binary starts with an
empty `PATH`. The UCRT (`api-ms-win-crt-*`) stays dynamic and should — it is
part of Windows, not something llvm-mingw supplies. Note also what is *not* in
that list either way: no `vcruntime140.dll`, no `msvcp140.dll`. No MSVC
runtime is involved at any point.

Two things that look like they should work and do not: `-C link-arg=-static`
(rustc has already committed `-lunwind` to the dynamic bracket by then), and
adding a `-L` path that contains only `libunwind.a` (this one does work, but
only by curating a directory holding a copy of a file from a specific
llvm-mingw build — nothing in the repo can create or refresh it).

## Verified

`cargo build --workspace` completes clean on this toolchain — all nine crates,
including the Bevy client and `soils-terrainlab` — and the resulting
`soils-server.exe` starts with llvm-mingw absent from `PATH`. The only
diagnostic is a pre-existing `dead_code` warning on `AuthPool::peak` in
`soils-server`. A clean build takes roughly 40 minutes, dominated as always by
compiling Bevy at `opt-level = 3`; see [`build-times.md`](build-times.md) for
what that number is made of and how the incremental loop was shortened.

Because the `rustflags` above are keyed to the target, changing them
invalidates every crate in the graph — expect a full rebuild, not an
incremental one, after editing that block.
