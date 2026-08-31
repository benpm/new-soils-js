# Repo hygiene

What was removed, what was kept, and the settings that keep the working tree
cheap to search. Written during the 2026-08-25 audit; update it when branches
are retired or the ignore rules change.

## Branch cleanup, 2026-08-25

The repo had accumulated eight branches and four open pull requests, most of
them stale copies of work that had already landed or had never been finished.
The end state is three: `master`, `ui-inventory` (open PR), and `audit`.

Nothing was deleted before its unique commits were examined. Where a branch
carried real work, that work was cherry-picked onto `audit` rather than merged,
so the history stays linear and the revert-and-re-apply tangle between two of
the branches did not have to be untangled.

### Kept — work salvaged onto `audit`

| From | What | Why it was worth keeping |
|---|---|---|
| `worktree-uniform-light` (PR #4) | `ChunkLight` as `Uniform(u8) \| Dense(..)` plus a full-sky flood fast path | Every all-air open-sky chunk paid a 32768-cell flood and stored 32 KB purely so neighbours read correct border light. Full-sky flood ~953 µs → ~188 µs, and uniform chunks store one byte. Oracle-equivalence property tests included. **Its client half was dropped** — it patched the padded light volumes, which no longer exist (see below). |
| `test/walk-physics-scenarios` (PR #5) | "Every `Edit` gets exactly one response" + `robustness.rs`, `movement_perf.rs` | A real bug, still live on `master`: the Edit handler's entity-not-queryable arm dropped the message with no reply, so a client in the post-login pre-flush window waited forever. Edits have no snapshot echo to self-heal from. The branch's third commit rewrote the shared test harness and was **not** taken — master's harness already provides everything these tests call. |
| `terrainlab-node-terrain-designer` | Multi-mode `Noise` + `Fractal Noise` nodes (10 modes, CPU+GPU parity) | Landed after PR #2 merged and was never pushed onward. Safe to take: it adds a `TerrainGraph::deterministic()` gate that rejects the new f32 design-only nodes from game paths, so chunk generation is unchanged and no golden re-pinning was needed. |
| `.claude/worktrees/light-pad-cache` (uncommitted) | Criterion benches for the chunk codec and the greedy mesher | Never committed anywhere; would have been lost with the worktree. |

### Dropped — the work no longer applies

**`worktree-light-pad-cache` (PR #3), a version-keyed cache for padded light
volumes.** It memoized a rebuild that master no longer performs: the stream
pipeline moved the light flood onto the GPU over a pooled cache and deleted
`build_padded` outright. `soils-client/src/light.rs` says so in as many words —
*"pads died with the padded volumes"*. Cherry-picking it would have
resurrected the machinery it was written to speed up.

The same reasoning removed the client half of PR #4, which patched the same
dead path. Its sim and server halves are independent and were kept.

This is the case for reading a stale branch before merging it. Both PRs looked
like clean perf wins in their commit messages, and one of them was optimizing
code that had been deleted.

### Removed

| Branch | PR | Why |
|---|---|---|
| `spacetimedb-integration` | #6 (merged) | Merged into `master` as `ed29e92`. |
| `terrainlab-node-terrain-designer` | #2 (merged) | Base merged as `5a290af`; the later noise-node commits were salvaged first (above). |
| `claude/rust-bevy-port-ftJBP` | #1 | Two commits, and the second reverts the first ("probe MCP write permission (temporary)" → "remove MCP write-permission probe"). Net content: nothing. 121 commits behind. |
| `worktree-light-pad-cache` | #3 | Premise obsolete — see above. Nothing salvaged. |
| `worktree-uniform-light` | #4 | Sim/server halves salvaged. The branch also carried a walk-physics test commit and its own revert, which is why the useful commit was cherry-picked rather than the branch merged. |
| `test/walk-physics-scenarios` | #5 | Work salvaged. |
| `rust` | — | Remote already deleted; the local ref was a leftover tracking a branch that no longer existed. |

`.claude/worktrees/light-pad-cache` is a 7.8 GB second checkout of the whole
repo. It duplicates every source file, so every repo-wide search returns each
hit twice, and it is easy to edit the wrong copy by accident.

Everything unique to it has been salvaged (the two benches above), so removing
it loses nothing:

```
git worktree remove --force .claude/worktrees/light-pad-cache
```

`--force` is needed because the worktree still holds the uncommitted Cargo.toml
edits that wired those benches up — they are reproduced on `audit`. Until it is
removed, the local `worktree-light-pad-cache` branch stays checked out there and
cannot be deleted; its remote is already gone.

## Keeping the working tree cheap to search

`.gitignore` covers what must not be committed. `.claude/settings.json` covers
something different: what an agent should not *read*, because reading it burns
context without informing anything.

Denied there:

* `.claude/worktrees/**` — a second copy of the repo, if one is ever created
  again. Doubles every search result and invites editing the wrong copy.
* `crates/soils-stdb/src/module_bindings/**` — 29 files, ~4,200 lines of
  SpacetimeDB-generated bindings. They are checked in so a normal build needs
  no CLI, but nothing in them is worth reading, and they used to swamp any
  grep for a table or reducer name.
* `recordings/**`, `artifact/**`, `node_modules/**` — captures, generated
  pages, and dependencies.

`target/`, `.tools/` and `recordings/` are deliberately **not** denied, though
the obvious advice says to deny all three. Build artifacts get inspected (does
the exe exist, how big is it), the pinned SpacetimeDB CLI lives in `.tools/`,
demo takes have to be listed and probed before they are published, and
Glob/Grep already skip gitignored paths — so denying them buys nothing and
blocks real work.

Each of those was learned the same way, twice: the first version of this file
denied `target/` and blocked a build-artifact check within the hour, and the
`recordings/` rule blocked the inspection of a demo take that turned out to be
broken. The general lesson is that a deny list should name things nobody needs
to look at, not things that merely *sound* like build output. A gitignored path
is not automatically a path an agent should be blind to.

The same file disables plugins that have no bearing on a Rust workspace
(`playwright`, `clangd-lsp`, `skill-creator`, `plugin-dev`,
`claude-md-management`). They cost context on every turn and were never used
here.

## Secrets

`~/.claude/settings.json` held a GitHub personal access token in plaintext
under `env`. Anything that reads that file — including an agent asked to check
its settings — pulls the token into a transcript. It has been removed.

If the GitHub MCP needs it back, set it in the OS environment instead of in a
config file that gets read:

```powershell
[Environment]::SetEnvironmentVariable('GITHUB_PERSONAL_ACCESS_TOKEN', '<token>', 'User')
```

A token that has ever appeared in a transcript or a config file should be
rotated at <https://github.com/settings/tokens> rather than reused.
