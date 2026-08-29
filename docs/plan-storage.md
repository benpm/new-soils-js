# Storage: one file format, one cache policy, everything world-shaped on top

**Status:** phase 1 shipped 2026-08-28 on `ui-inventory` — `paged` + `store`
extracted, block data and containers built on them, chests working end to end.
Phase 2 (the remaining tenants) is designed here and not built.

---

## 1. What was already there

The brief was "a chest points at a data location, streams it from disk, keeps it
in memory until a write-out is necessary; do this kind of caching with all
data." The interesting thing about that brief is that the chunk pipeline had
already been doing exactly it, for voxels, since phase 6:

| The brief | Where it already lived |
|---|---|
| a pointer to some data location | `region.rs` — a 4096-entry `u32` table at the head of each region file; `0` absent, `1` empty, anything else a byte offset |
| streamed straight from disk | `read_chunk` — seek to the offset, read a length, inflate |
| copied to CPU | `ChunkEntry.volume`, resident in `World::chunks` |
| held in cache until write-out is necessary | `dirty` flag, flushed on an interval, on eviction, and at shutdown |
| and not before | `PersistHandle` — a background writer thread, so no write is ever on the tick |

So the work was not to invent a caching layer. It was to notice that the one in
`region.rs` and `world.rs` was welded to `ChunkVolume`, and to unweld it —
because the second thing that needs it (what is inside a chest) is not a voxel,
and the third and fourth will not be either.

The result is two modules that know nothing about the game:

* **`soils-server/src/paged.rs`** — the file format. Slotted, append-and-repoint,
  zlib blocks, temp-file compaction.
* **`soils-server/src/store.rs`** — the policy. `Store<V>`: fault in on miss,
  mutate in RAM, dirty, flush, pin, evict, with counters.

`region.rs` is now the *chunk-shaped* layer over `paged` — which slot a position
maps to, that an all-Air chunk is a sentinel rather than 32 KB of zeroes, and
that a pristine chunk is worth pruning because worldgen can reproduce it. Its
six tests were not touched by the extraction, which is the evidence that the
format did not move.

---

## 2. `paged` — the format

```text
[ 4096 x u32 entry ][ u32 len ][ zlib payload ][ u32 len ][ zlib payload ] ...
  ^ the pointer table                ^ a block, addressed by an entry
```

An entry is `ABSENT` (0), `SENTINEL` (1 — present, empty), or a byte offset. The
high bit is a caller-defined `FLAG`; offsets stay far below 2 GB, so it is free.

**Why append-and-repoint rather than update in place.** A changed slot appends a
fresh block and rewrites its 4-byte entry. The entry write is the commit: a
crash before it leaves an unreferenced block and the old content intact, and a
crash after it leaves the new content and no dangling reference. There is no
window in which a reader can see a half-written value. The cost is leaked bytes,
which `compact` reclaims on world open — temp file, atomic rename — once a file
crosses 25% waste and 64 KB.

**Why the caller supplies compaction's `keep` predicate.** Chunks keep only
*edited* entries: a pristine chunk is byte-reproducible from the world identity,
so pruning it costs a regeneration and saves its bytes. Block data keeps
everything — nothing regenerates a chest. Same compactor, two policies, and
neither is hardcoded into the format.

**Why 4096 slots and not a directory.** Because addressing is the caller's, and
the first caller had a natural fixed key space: 16x16x16 chunks per region. That
turns out to be the right unit for the second caller too (§4). A structure whose
keys are *not* spatial does not fit this file, and §6 says so rather than
pretending otherwise.

---

## 3. `store` — the policy

```rust
pub trait Codec: Sized {
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Option<Self>;
    fn is_empty(&self) -> bool;
}

pub struct Store<V> { /* pages, memoised headers, stats */ }
```

* `get` / `get_mut` — fault in on miss; `get_mut` is the commitment to a write,
  so a path that only *might* change something checks with `get` first. An
  unwritten page reads as `V::default()`: "nothing here" and "nothing yet" are
  the same answer to a reader.
* `pin` / `unpin` — hold a page against eviction while something is using it. An
  open chest pins its page; the alternative is a container that empties itself
  because its page left memory mid-session.
* `take_dirty` — encoded bytes for the background writer. Nothing here touches
  disk.
* `tick_lifecycle(ttl)` / `evict(pos)` — drop idle or orphaned pages, flushing
  the dirty ones on the way out.
* `stats()` — hits, misses, loads, writes, evictions, corrupt. Logged on the
  flush interval once anything has been stored. A cache with no visible hit rate
  is a cache nobody can tell is broken.

Three decisions worth keeping:

**The pointer table is memoised, per region, not per page.** A miss consults an
in-memory table; only a real block opens the file. This is what makes it
acceptable to ask "does this block have data?" on *any* block break, which is
what the spill path needs. `the_pointer_table_is_read_once_per_region_not_once_per_page`
pins it.

**An empty value clears its slot rather than storing nothing.** A chest placed
and never used costs no bytes; a chest emptied stops costing them. `ABSENT` and
`SENTINEL` are different states precisely so this is expressible.

**An undecodable payload reads as empty, and counts.** A format change must not
make a world unopenable. It is logged and counted (`stats().corrupt`) rather
than panicked on — but it is *counted*, so silently losing data is at least
visible.

---

## 4. Block data — the first new tenant

A voxel is one byte, so "this is a chest" is all the world array can say. What
is *in* it lives in `soils_sim::block_data`:

```rust
pub enum BlockData { Container(Inventory) }
pub struct ChunkData { blocks: BTreeMap<LocalPos, BlockData> }
```

**Keyed by position, not by an allocated id.** No id to hand out, no free list,
and no way for the side table to disagree with the world about which block it
describes. Break the block, drop the entry.

**One page per chunk, at the same slot index as the chunk's voxels.** `r_0_0_0.bin`
slot 37 and `b_0_0_0.bin` slot 37 are the same chunk seen two ways. A caller
that knows where a chunk lives already knows where its block data lives — no
index, no second key space to keep coherent with the first. Residency piggybacks
the chunk's: evicting a chunk evicts its page, because block data outliving its
voxels is a leak with extra steps.

**`BTreeMap`, not `HashMap`.** Encoding is deterministic, so identical contents
produce identical bytes. That is what lets a test compare a round-trip and what
would let a writer skip an unchanged rewrite.

Which blocks hold things is data, not code: `container: 27` in `blocks.yaml`.
Presence of the key is what makes a block openable — there is no separate
boolean to disagree with it. Wooden Crate is 27 slots, Clay Pot is 9.

---

## 5. Containers over the wire (protocol v5)

```rust
ClientMsg::OpenContainer { pos }
ClientMsg::CloseContainer
ClientMsg::TransferItem { from: SlotRef, count }   // SlotRef = Pack(u16) | Container(u16)

ServerMsg::ContainerUpdate { pos, slots }
ServerMsg::ContainerClosed { pos }
```

**A transfer names what to move, not where to put it.** The destination is
whichever side `from` is not. Two players may hold one chest open, and neither
can be right about which slot the next stack lands in — so the client does not
get to model the server's stacking rules. It also means the client never needs
to know them.

**Opening is not a lock.** Both viewers see every change, because the server
pushes `ContainerUpdate` to everyone whose `open_container` matches. Whole state
rather than a delta, for the `InventoryUpdate` reason and more so: a lost delta
on a shared object desyncs two people at once.

**The server decides the panel is open.** The client asks; only `ContainerUpdate`
says it happened, and only `ContainerClosed` takes it down. There is no
optimistic path.

**Reach is re-checked on every transfer**, not just on open — otherwise a chest
stays lootable for as long as the client keeps quiet about walking away. It uses
`soils_sim::within_reach`, split out of `validate_edit` so placing and opening
cannot drift apart. A server that reach-checks placing but not opening is a
server you can loot a chest through a wall with.

**Breaking a container spills it.** Everything it held drops as world items and
every viewer is closed. This is the correctness cliff: without it, breaking a
chest either voids its contents or leaves orphan data that the next chest built
on that voxel inherits.

Known gap: a *script* edit (`run_scripts`) that removes a container block does
not go through this path. Recorded in `Tasks.md`.

---

## 6. Phase 2 — the remaining tenants

**Fits the store as-is** (a `Codec` impl and a prefix letter):

* *Signs, furnaces, spawners, growth timers* — more `BlockData` variants. Each
  is a format commitment, which is why the type is a closed enum.
* *Per-chunk gameplay state* — mob spawn budgets, claim ownership, visited
  flags. New `Store<T>` with its own prefix; the addressing is already right.
* *Nav grids*, if they ever stop being cheap to rederive. Today `World::navs` is
  a version-keyed derived cache, which is the correct answer for data that costs
  ~1 ms to rebuild and would cost more than that to read. Worth revisiting only
  if the grid grows.

**Does not fit, and should not be forced.** Player profiles and inventories are
not spatial. A `paged` file is a fixed table addressed by position; a player key
would need a directory or a hash table with chaining — a second format, wearing
the first one's name. Two honest options:

1. One file per account under `worlds/<name>/players/`, with the same write-back
   discipline (dirty in RAM, flushed by the persist thread) but a trivial
   whole-file codec. This is what the `Tasks.md` item "inventory does not survive
   logout" actually wants, and it is small.
2. Keep owning it in SpacetimeDB, which already has `SaveInventory` and
   `SaveProfile`, and treat the disk path as the offline fallback.

The point of writing this down is that "cache all data the same way" is right
about the *policy* — write-back, pinned while in use, evicted on a timer,
counters visible — and wrong about the *format*, for anything whose key is not a
place. Sharing the policy is free; sharing the file is not.

---

## 7. What is tested

| Claim | Where |
|---|---|
| absent / sentinel / payload are three distinct states | `paged::tests` |
| a rewrite repoints; a clear returns a slot to absent | `paged::tests` |
| compaction reclaims leaks and honours `keep` | `paged::tests`, `region::tests` |
| the chunk format did not move | the six pre-existing `region::tests`, unchanged |
| a page survives a flush and a cold reopen | `store::tests` |
| eviction writes a dirty page out rather than losing it | `store::tests` |
| a pinned page is not evicted | `store::tests`, `world::tests` |
| an emptied page clears its slot | `store::tests`, `world::tests` |
| the pointer table is read once per region | `store::tests` |
| an undecodable payload reads as empty | `store::tests` |
| block data survives eviction and reloads | `world::tests` |
| a crate holds items and gives them back | `tests/containers.rs` |
| a transfer never creates or destroys an item | `tests/containers.rs` |
| breaking a full crate spills everything and closes it | `tests/containers.rs` |
| an ordinary block is not a container | `tests/containers.rs` |
| a crate out of reach cannot be looted | `tests/containers.rs` |
| both viewers of one crate see every change | `tests/containers.rs` |
| contents survive a server restart | `tests/containers.rs` |
| the panel is open exactly while the server says so | `inventory::container::tests` |
