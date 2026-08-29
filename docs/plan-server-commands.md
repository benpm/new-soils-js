# Plan: server slash commands

**Status:** design only, nothing implemented. Written 2026-08-29 on
`ci-pipelines`. Companion to `plan-game-systems.md` (authority, protocol) and
`dev/server-tick.md` (tick budget, determinism).

---

## 0. The thing this plan has to say first

**A console already exists, and almost none of it is a server command.**
`crates/soils-client/src/console.rs` opens on `/`, takes text, and dispatches.
That is the shape of the feature and none of its substance:

| Typed | What a player assumes | What actually happens |
|---|---|---|
| `tp x y z` | you teleport | the local transform is written; `reconcile_self` drags you back |
| `daytime t` | the time changes | *your* sky changes, nobody else's |
| `fog` `ao` `gi` `sens` | — | genuinely client-local, and correctly so |
| `warp` `spawn` | — | the only two that reach the game server |
| `say` | chat | goes to SpacetimeDB, deliberately around the game server |

`tp` and `daytime` are lies told to one client. They were useful lies — this
was a debug box and it did its job — but a player typing them into a
multiplayer server is being deceived. `tp` is not even a *stable* lie:
`reconcile_self` (`player.rs:299-310`) adopts the server position outright once
divergence passes 1.0, so a client-side teleport snaps back within a tick or
two. It only appears to work across short distances.

Three structural gaps sit behind that, and each is a thing to build rather than
a thing to fix:

1. **There is no command message.** `ClientMsg` has 13 variants
   (`soils-protocol/src/messages.rs:61`) and none carries a command.
2. **There is no way to answer a player.** `ServerMsg::LoginError { message }`
   is the only free-text server-to-client message, and the login screen
   consumes it. `run_command`'s `_ => {}` makes a typo and a success
   indistinguishable; every parse failure today is a silent no-op.
3. **There is no authority concept.** Grepping `crates/` for
   `is_admin|role|moderator|kick|ban` returns nothing. A `Client`
   (`app.rs:113-195`) is an `account: String`, and that is the whole of its
   identity.

The interesting work is in 2 and 3. Parsing is not the hard part, and this plan
spends very little on it.

---

## 1. Decisions taken up front

Settled before design; not relitigated below.

* **Roles live on the account** — `Player | Mod | Admin`, persisted, checked at
  dispatch.
* **One registry, hybrid.** A single `/` prompt and one merged command table,
  each entry known to run either client-locally or on the server. Players never
  need to know which is which, and `/help` is one listing.
* **v1 surface:** movement (`tp`, `spawn`, `warp`), items (`give`, `clear`,
  `drop`), world (`time`, `fill`), admin (`list`, `kick`, `role`), and an
  admin-only mass-voxel-removal tool obtainable only by command
  ([§7](#7-the-excavator)).

One command from the original brief, **`/regen`, is cut from v1** — see
[§5.5](#55-why-regen-is-not-in-v1). Everything else ships.

---

## 2. The wire format: send the line, parse on the server

`ClientMsg::Command { line: String }`. The server owns the grammar; the client
is a pipe.

The argument is not "one protocol bump forever" — `PROTOCOL_VERSION` is checked
at login (`app.rs:1099`), so client and server are already locked in lockstep
and there are no old clients to serve. The argument is **drift**. A typed
variant per command puts the grammar in two crates: the client parses
`/tp 10 20 30` into `Tp { x, y, z }` and the server matches it. Add the
player-name form `/tp ben` and you now edit both, and a mismatch fails
*silently* — the client sends nothing and the player sees no error. With a raw
line, arity, coercion, defaults, aliases and error text live in exactly one
place. Parse cost is irrelevant: a 512-byte `split_whitespace` behind a token
bucket is nanoseconds.

```rust
/// A console line, verbatim, minus the leading `/`. The server owns the
/// grammar: there is no client-side parse to trust, and adding a command
/// never touches this enum.
Command { line: String },
```

* The arm goes **after** the pre-auth wildcard at `app.rs:1261`
  (`_ if !clients.0[&id].authenticated => {}`), which auto-gates it. Free, and
  easy to get wrong.
* Cap at `MAX_COMMAND_LEN = 512` — the same number as `MAX_CHAT_LEN`
  (`stdb/soils-module/src/lib.rs:23`), for the same reason — and charge
  `ui_tokens` (`UI_RATE`, `app.rs:97`).
* **A command the client does not recognise is still sent**, so the server can
  name it back instead of the client swallowing it.

### Bound the socket while you are here

`Command` is the first **unbounded-length** client-to-server field, and
`pump_connection` (`lib.rs:470`) calls `accept_async` with tungstenite's
defaults — `max_message_size` 64 MiB. Every existing `ClientMsg` is at most
about a kilobyte, so this is already latent exposure through `Warp { world }`
and `Login { password }`; a free-text console line turns an oversight into a
decision. Use `accept_async_with_config` with `max_message_size` and
`max_frame_size` of 256 KiB. One line, and it is the difference between "the
server parses the line" being safe and being a memory amplifier.

### The registry: pushed as data, not shared as a const

```rust
// ServerMsg — pushed once, immediately after `Init`.
CommandSpecs { specs: Vec<CommandSpec> },

pub struct CommandSpec {
    pub name: String,          // no leading slash
    pub aliases: Vec<String>,
    pub usage: String,         // "<x> <y> <z> | <player>"
    pub summary: String,
    pub min_role: Role,
}

/// Commands the client answers itself and never sends. Declared in the shared
/// crate so the *server's* registry constructor can assert it defines none of
/// them: two definitions of one name is a command that does different things
/// depending on which half of the build you look at.
pub const LOCAL_COMMANDS: &[&str] =
    &["fog", "ao", "gi", "light", "sens", "sensitivity", "loadradius", "playerlight"];
```

Pushing the table rather than compiling it into both sides is what makes the
hybrid registry honest: the server's command set *is* the command set, and
`/help` cannot go stale. Routing in `run_command` is then one rule — token 0 in
`LOCAL_COMMANDS` is handled locally and never sent; everything else is sent.

`/help` merges the local table with the pushed specs, sorted, with
above-your-role entries **dimmed rather than hidden**. Roles are not secrets,
and hiding them only produces "why doesn't /kick work". Send all specs
regardless of role and carry `min_role` so the client can grey them.
Tab-completion is prefix match over the same merged list.

---

## 3. Answering the player

```rust
// ServerMsg
/// Free text for the player's console: command output, command errors, and
/// unsolicited server notices. One message per logical reply, so a 20-line
/// `/help` is one frame and one scrollback insertion rather than twenty the
/// client has to guess the boundaries of.
Notice { level: NoticeLevel, lines: Vec<String> },

pub enum NoticeLevel { Info, Success, Error }
```

**`Notice`, not `CommandReply`.** A long `/fill` finishes seconds after the
prompt closed. Modelled as a *reply to a command*, that late arrival needs a
correlation id and a pending-request table on the client, for nothing.
Modelled as "the server said something", it is just another line — and it
gives a home to things nobody asked for: *you were promoted to Mod*, *your fill
completed: 41 231 voxels*.

It also gives `EditRejected { seq }` a reason channel **without changing
`EditRejected`**, which should stay tiny — it is a control message on the
rollback path. Not wired in v1; the door is open.

`Vec<String>` rather than one newline-joined string, because scrollback wants
per-line entries with their own arrival time and colour.

### Sanitize at the producer

This is the security note, and it is new. `LoginError` is server-authored
today. `Notice` will carry **attacker-chosen** content: account names are
player-picked, up to 32 bytes (`auth.rs:31`), and `/list` and `/kick` echo
them. A name containing a newline lets one player forge server lines in
everyone else's console. Every line goes through one helper:

```rust
/// Flatten a line for the console: control characters out, length capped.
/// Account names reach this from `/list` and `/kick`, and a name holding a
/// newline is a player forging server output in someone else's scrollback.
fn notice_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_NOTICE_LINE_LEN).collect()
}
```

### Rendering: scrollback, merged with chat

```rust
/// Everything the server has said to us, newest last. Bounded: a console is a
/// tail, not a log file.
#[derive(Resource, Default)]
pub struct ConsoleLog {
    lines: VecDeque<(f32, NoticeLevel, String)>,
}
```

* **Console closed** — the existing `ChatHud` node (`hud.rs:15-65`) renders the
  merge of `Social.chat` and the `ConsoleLog` entries younger than ~10 s.
  *Merge them.* A player should not have to look in two places for "text the
  game said", and the two sources have identical presentation needs. Chat stays
  1 Hz-polled, notices land at tick rate, both feed one list rebuilt per frame.
* **Console open** — the same node expands to the last ~16 lines regardless of
  age, PageUp/PageDown scroll an offset, and the existing `ConsoleBar` input
  sits below it.

Colour by level: Info white, Success green, Error red.

Wire it in `server_msg.rs` by adding a writer to the `Routed` SystemParam — the
struct that exists at `server_msg.rs:217` precisely because the router hit
Bevy's 16-parameter ceiling, and which has headroom now.

### Do **not** add `UiMode::Console`

`plan-ui.md` §2 specified a `UiMode::Console` variant. **It should not be
built, and this plan supersedes that line.**

`UiMode::wants_cursor()` returns `!matches!(self, Playing)`, and
`apply_cursor_mode` is the sole writer of `CursorOptions`. A `Console` variant
would therefore *free the pointer while you type*, and `click_to_grab` would
re-lock it on the next click. That is a regression, not a cleanup.

The current arrangement is already right: gameplay systems are gated on
`ui::playing` **and** `console::console_closed` together (`main.rs:323-355`),
`predict_and_send` included, so an open console suppresses input *and* sends
none, while the pointer stays locked where a typist wants it. The console is a
modifier over `Playing`, exactly like the Alt cursor override — not a mode. A
`Console` variant would also force someone to define what
Console-plus-Inventory means, which is a question with no useful answer.

Keep the `pending.clear_latches()` call on open (`console.rs:81`): it stops a
jump queued that frame from firing on close.

---

## 4. Roles

### Not a column on the SpacetimeDB `Account` table

Two facts from the code decide this:

1. **`Account` is private.** `stdb/soils-module/src/lib.rs:71` is
   `#[table(accessor = account)]` with no `public`, unlike `world` and
   `player_profile`. The game server cannot read it from the SDK cache at all;
   it only asks `verify_login` for a verdict.
2. **Adding a column to a live table is a breaking migration**, and the module
   says so itself at `lib.rs:157`: adding a column *"is a breaking schema
   change, and this module's only answer to that is `--delete-data=always`,
   which destroys the account table. Adding a **table** migrates additively."*
   That comment exists because someone already learned it.

So `accounts.bin` is authoritative for role, and SpacetimeDB gets a **new
additive public table** as a mirror:

```rust
/// An account's permission level. A separate table from `Account` because that
/// one is private (the server can only ask `verify_login` about it) and adding
/// a column to it is the one migration this module cannot perform without
/// destroying every password. Public: a role is not a secret, and readability
/// from the SDK cache is what lets a second game server learn it without a
/// round trip.
#[table(accessor = account_role, public)]
pub struct AccountRole {
    #[primary_key] pub account: String,
    pub role: u8,
    pub granted_by: String,
    pub granted_at: Timestamp,
}
```

with a `set_account_role` reducer guarded by the existing `require_server`
allowlist (`lib.rs:237`). A player must never promote themselves, so promotion
flows player to game server to module, as `link_identity` already does.

### The file format, and the trap in the obvious approach

`Accounts::load` (`auth.rs:83`) already has a two-rung fallback: current
`HashMap<String, Stored>`, then legacy `HashMap<String, u64>`. **Adding a third
rung the same way is unsafe**, and this is the most dangerous thing in the plan.

`soils_protocol::decode` (`messages.rs:40`) is:

```rust
bincode::serde::decode_from_slice(bytes, config()).ok().map(|(v, _)| v)
```

It **throws away the consumed length**. Bincode is not self-describing, and a
`Record { stored, role }` differs from a bare `Stored` by one varint byte per
entry — so decoding an old file as the new type can consume the next entry's
key-length byte as a role discriminant and, with the wrong luck, *succeed on a
prefix* and return a silently truncated map. That map is then written back over
the real one, and every account and password on the server is gone.

Use a magic prefix, so an absent tag **is** the version check:

```rust
/// On-disk tag for the roles-bearing account file. Files from older builds
/// have no tag, so absence is the version check — bincode is not
/// self-describing, and probing by "try decoding it as the new type" can
/// succeed on a prefix of the old one and silently return a corrupted map,
/// which is then saved back over the real accounts.
const MAGIC: &[u8; 4] = b"SLA1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Record { pub stored: Stored, pub role: Role }
```

`load` reads: tagged, decode `HashMap<String, Record>`; untagged, try
`HashMap<String, Stored>` then `HashMap<String, u64>`, both landing as
`Role::Player`. `save` prepends `MAGIC`. The first successful save after
upgrade *is* the migration; there is no separate step.

While in there: `save` overwrites in place with `std::fs::write`. Now that the
file carries roles as well as verifiers, make it write-temp-then-rename. A torn
write loses passwords today; this is the moment to fix it.

### The `Role` type

```rust
/// Account permission level.
///
/// **Declaration order is the permission order.** `Ord` is derived and every
/// gate is `caller.role >= spec.min_role`, so inserting a variant in the
/// middle silently re-grades every account above it. Append only, exactly like
/// the message enums.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role { #[default] Player, Mod, Admin }
```

Pin it with a test: `assert!(Role::Player < Role::Mod && Role::Mod < Role::Admin)`.

### Seeding the first admin

`ServerConfig` gains `admins: Vec<String>`, read from `SOILS_ADMINS` beside the
existing `SOILS_*` block in `main.rs`. The semantics are a **floor, not an
assignment**:

* A listed name is `Admin` whenever it logs in, whatever the file says. That is
  what makes it a recovery path: an operator who `/role`s themselves down
  restarts with the variable and is back in.
* On login, if the floor exceeds the stored role, persist the promotion — so
  removing the variable later does not demote anyone, and grants made by that
  admin outlive it.

Promote on login rather than at boot, because an account cannot exist without a
password and a boot sweep could only touch accounts that already exist.

Effective role is `max(env_floor, file_role, stdb_role_if_cache_is_warm)`. The
stdb read is an upgrade, never a downgrade: `link.profile()` reads the local
SDK cache, which the login path already knows may be cold (`app.rs:1163`), and
a cold cache reading as `Player` would silently demote an admin at exactly the
moment a server restarts.

`Client.role` is set once in the join path beside `c.account` (`app.rs:1147`)
and read at dispatch — no lock and no I/O on the tick.

---

## 5. The expensive commands are one problem, not three

`/fill` and the excavator are the same thing: a bounded region of voxel edits
far too expensive for one tick. They get one mechanism.

### 5.1 The tick budget is not 50 ms

`app.rs:597` runs `ScheduleRunnerPlugin::run_loop(5ms)` with
`Time::<Fixed>::from_hz(SERVER_TICK_HZ)` = 20 Hz. A system that spends 50 ms
consumes the *entire* period, leaving nothing for `pump_chunk_jobs`,
`replicate_entities` or `world_lifecycle` — and Bevy's fixed accumulator then
runs `FixedUpdate` twice next frame to catch up, which compounds. **The real
budget for a background job is 1-3 ms per tick.**

(`dev/server-tick.md:3` says "64 Hz, 15.6 ms". That is stale — 64 Hz is
`TICK_HZ`, the *client* sim rate. It is exactly the doc someone will consult
while reasoning about this, so correct it in the same change.)

### 5.2 The thing that kills you is not lighting

`World::edit` (`world.rs:678`) does, **per voxel**:

```rust
entry.faces = face_mask(&entry.volume);        // full 32³ scan
light::apply_voxel_change(&mut lw, pos);       // incremental flood
for c in touched { self.rebuild_summary(c); }  // another 32³ scan per chunk
```

A `/fill` looping `World::edit` over a 64³ box is 262 144 × two full-chunk
scans — on the order of **17 billion voxel reads**. Lighting is not the
problem; the per-voxel chunk rescan is. **The unit of bulk work must be a
chunk, not a voxel.**

```rust
/// Set every voxel of the inclusive box `min..=max` that falls inside chunk
/// `cpos`. Returns how many actually changed.
///
/// Deliberately not a loop over [`edit`](Self::edit), which rebuilds the
/// six-face mask (a 32³ scan) and reruns light propagation *per voxel*:
/// filling one chunk through it costs about 32 000× what it should. Here the
/// mask, the summary and the light enqueue are paid once for the whole chunk.
pub fn fill_chunk(&mut self, cpos: IVec3, min: IVec3, max: IVec3, value: u8) -> u32
```

Two invariants fall out and must be stated:

* **`entry.edited = true` is load-bearing.** `pump_chunk_jobs` classifies
  chunks into `ChunkInfo::Pristine` vs `Edited`. Miss it and a client joining
  after the fill regenerates pristine terrain straight over your hole.
* **Do not enqueue light per chunk.** `light_queue` is a `Vec` with no dedup,
  jobs are batched by (x,z) column, and `apply_light_results` *requeues* any
  chunk whose `version` moved while the job was in flight (`world.rs:906`). A
  fill touching one column across several ticks therefore invalidates every
  in-flight job and **livelocks the light pipeline**. Process a job's chunks in
  column order and enqueue only when a whole column is finished; make
  `light_queue` dedup while you are there.

### 5.3 The job

```rust
/// Chunks a bulk edit may touch per tick. Each costs a face-mask and a summary
/// rebuild — two 32³ scans — plus one chunk encode per subscribed client.
const BULK_CHUNKS_PER_TICK: usize = 4;
/// Hard ceiling on one command's volume, checked at parse time.
const BULK_MAX_VOLUME: i64 = 1 << 20;
/// Outstanding bulk jobs per client. A second `/fill` while one runs is
/// refused, not queued: a queue is just a slower way to hand one player the
/// whole server.
const BULK_JOBS_PER_CLIENT: usize = 1;

struct BulkEdit {
    world: String, by: u16,
    min: IVec3, max: IVec3, value: u8,
    /// Chunks still to touch, grouped by (x, z) column — see §5.2.
    remaining: VecDeque<IVec3>,
    changed: u64, skipped: u32,
}
```

A `pump_bulk_edits` system, scheduled `run_scripts -> pump_bulk_edits ->
pump_chunk_jobs -> pump_deferred -> ...`: after `drain_inboxes` so a command
enqueued this tick starts this tick, before `pump_deferred` so the
`expose_neighbours` it triggers resolves the same tick, and before
`world_lifecycle` so the light work it queues is picked up by this tick's
`pump_light`.

Per chunk: `ensure_resident` — and if that returns `false` the chunk was never
persisted and would need off-thread generation, so **skip it and count it**.
That is both the right v1 answer and a safety property: `/fill` can only touch
world that already exists. Say so in the reply — *"filled 41 231 voxels in 39
chunks; skipped 7 not yet generated"*. Then `fill_chunk`, then
`expose_neighbours` (a fill can punch a seal exactly as an edit can), then
close any client's `open_container` inside it.

Two deliberate omissions, both of which want writing down:

* **Bulk edits destroy block data without spilling it.** `ClientMsg::Edit`
  spills a broken container's contents as `spawn_drop` entities
  (`app.rs:1381`); doing that for a fill would spawn thousands of replicated
  entities. Report it instead — *"destroyed 3 containers"*.
* **Bulk edits emit no script events.** The `Edit` arm pushes
  `ScriptEvent::Edit` per voxel (`app.rs:1421`); a million events would blow
  `ScriptEvents` and every `on_edit` handler. Add a coarse
  `ScriptEvent::BulkEdit { min, max }` later if a script ever needs it.

### 5.4 The broadcast: `Manifest`, not `Edit`

This needs **no protocol change**, because the client already does the right
thing. `demand.rs:158-179` handles `ChunkInfo::Edited` for an already-resident
chunk by dropping its optimistic overlay and materialising the fresh payload —
commented *"the payload already includes any edits we had overlaid"*.

So each tick, for the chunks just finished, send one `ServerMsg::Manifest` per
client covering exactly the chunks it subscribes to. A 32³ chunk is 32 768
`Edit` messages at ~14 bytes against one LZ4-and-palette payload of a few
kilobytes. The requester gets it too — unlike a single edit, they applied
nothing optimistically.

One wrinkle: `send_world_subscribed`'s `except: u16` cannot express "exclude
nobody" (`u16::MAX` is reachable from the wrapping id counter). Change it to
`Option<u16>` at its three call sites, or write this loop inline.

### 5.5 Why `/regen` is not in v1

Every other command writes to resident chunks. `/regen` needs off-thread
regeneration *and* an overwrite, and `World::adopt` (`world.rs:614`) explicitly
refuses to overwrite a resident chunk — "the resident chunk wins" — which is
the invariant that makes the generate-then-edit race safe. Changing it is a
separate and riskier piece of work. Cut it; ship the other eight commands.

### 5.6 `/time`

`Clock` is one global resource written only by `tick_clock` (`app.rs:2765`) and
**shared across all worlds**, so `/time` in world `arena` also changes the sky
in `default`. That is a pre-existing wart, not one this introduces — say so in
the command's help rather than fixing it here. Set `clock.daytime` and
broadcast `ServerMsg::Time` immediately rather than waiting up to a second for
`tick_clock`.

---

## 6. `/tp`, done honestly

Server side, mirroring the `Warp` arm (`app.rs:1698`): set `sim.0.pos`, zero
`vel`, set `c.center`, then `resubscribe` (`app.rs:1876`) to manifest the new
area and unload the old.

**`reconcile_self` needs no change.** The next snapshot carries the new
position against `last_input_seq`; the client finds its ring entry, sees
divergence far past `RECONCILE_EPSILON`, rewinds to the server position and
replays its unacked inputs from there. That is exactly right — and because
`predict_and_send` is gated on `console_closed` (`main.rs:355`), no inputs were
sent while the console was open, so the ring is nearly empty. The cleanest
possible case.

The client work is subtraction: delete the local `tp` arm. Its
`streaming.last_chunk = None` is vestigial — `Streaming.last_chunk` only sizes
the HUD estimate and the `ViewRadius` send; the server owns subscription
membership and `resubscribe` does the real work.

### The part nobody asks about: teleporting into unloaded space

`World::voxel` returns air for non-resident chunks, on the server *and* the
client. Teleport 500 blocks and both sims agree you are in a vacuum, so you
fall; when the chunks arrive you are inside rock.

Setting `sim.flying = true` **does not work**, and the reason is worth knowing:
`reconcile_self` takes `flying`/`grounded` from the client's recorded state,
never from the server (`player.rs:327-328`, with a comment explaining that
taking it from the prediction would double-apply fly toggles). The server has
no way to tell the client it is flying. Do not open that.

Instead, a one-shot settle:

```rust
/// A teleport whose destination was not resident yet. Held for at most
/// `SETTLE_TICKS`; the moment the destination chunk exists, a player standing
/// inside solid rock is lifted to the first free space above it.
///
/// The alternative looks more honest — move them and let physics sort it out —
/// but it cannot: unloaded space reads as air on *both* ends, so they fall
/// through a world that has not arrived and wake up embedded in a hillside.
#[derive(Resource, Default)]
struct PendingSettles(Vec<(u16, u64)>); // (client, deadline tick)
```

Move them immediately so the camera and streaming go, register a settle, and
let a system after `pump_chunk_jobs` lift them once the chunk exists. The
ordinary reconcile then corrects the client. Past the deadline, drop it.

---

## 7. The excavator

### The item is a YAML line

`crates/soils-sim/items.yaml` has `tools: []` and a header documenting that
"authoring the first fruit is a YAML edit", with ids being the declaration
index. So:

```yaml
tools:
  - name: Excavator
    tile: 0
    function: mine
```

That is `ItemKind::Tool(0)`, and it inherits `max_stack() == 1`, an
`ItemClass`, a hotbar binding and `in_ring()` for free. `items.yaml` is
`include_str!`'d into `soils-sim`, which both binaries link, so client and
server cannot disagree about the id. **No protocol change, no registry
change.**

Add a named constant so nothing spells `Tool(0)`, plus a test asserting the id
still resolves to the name — that is the guard against someone reordering the
list:

```rust
/// The admin excavator. An id rather than a name lookup because the server
/// gates on it in several places, and a string compare against `items.yaml` in
/// any of them is one rename away from being a security hole.
pub const TOOL_EXCAVATOR: u16 = 0;
```

### Where the gate goes: obtaining *and* using

Both, and use-time is the load-bearing one.

* **Gating obtaining only is catastrophic.** The item is droppable, tradeable
  and storable in a chest. An admin drops one, a player picks it up, and a
  player holds a world-eating tool.
* **Gating use only is safe but incoherent** — an ex-admin's chest of
  excavators silently goes live again on re-promotion, which nobody will reason
  about correctly.

So gate four places: **creation** (only `/give` at `Admin` mints one; no
recipe, no drop table, no worldgen source), **use** (checked every time),
**pickup** (skip admin items for non-admins in the pickup loop), and
**demotion** (a `/role` below Admin sweeps that client's live inventory and
removes them, with a `Notice`).

The invariant worth wanting is *"only admins hold admin items"* — legible and
checkable. The invariant actually enforceable is *"only admins can use them"*,
because chests are out of reach. Have both, and be clear that the second is the
security boundary and the first is hygiene.

### Using it

```rust
/// Use the item in inventory slot `slot` against voxel `target`.
///
/// No `seq`: unlike `Edit` the client applies nothing optimistically — it does
/// not know the tool's radius rule and must not — so there is nothing to roll
/// back and nothing to correlate. The outcome arrives as a `Notice` and the
/// world change as a `Manifest`.
UseItem { slot: u16, target: [i32; 3] },
```

`slot` is an inventory index, not a hotbar index: the server has no hotbar, it
is client-only. Validation order, spending nothing until it must — rate token,
slot actually holds an admin item, `role >= Admin`, `within_reach` from the
**server-side** position (the same rule `Edit` uses), and the per-client job
cap. Non-admin and not-a-tool fail the same silent way; telling a player which
it was is telling them what to farm for.

**The radius is a server constant, not a client argument.** A client-supplied
volume is a client-supplied server outage. If a variable radius is wanted
later, make it `/excavate <r>` — a command, so it travels the channel that
already has a role check and a rate limit on it.

Client side, `edit_blocks` (`edit.rs:100-130`) gains one branch: if the
selected item is a `Tool` and the click is a break, send `UseItem` and apply
nothing locally.

---

## 8. `/kick`

Cleanup (`app.rs:1718-1759`) is driven only by a closed inbox, and the app
holds the `UnboundedReceiver<ClientMsg>` while the connection task holds the
sender — so the app cannot drop the sender itself.

Minimal correct mechanism: a `ServerMsg::Disconnect { reason }` on the reliable
lane, **recognised by the writer task**. `pump_connection`'s writer already
breaks when its channel ends (`lib.rs:487`, beside an arm commented "app
despawned the client"); teach it that seeing `Disconnect` means "send this,
then send a Close frame and stop", and have the read loop end when the writer
does. Then `in_tx` drops at function exit, phase 1 sees
`TryRecvError::Disconnected`, and the *existing* cleanup runs unchanged —
inventory save, profile save, `MarkAbsent`, entity despawn, refs released.

**One cleanup path. That is the entire point.** A kick that removed the client
inline would be a second copy of that logic, and a second copy is how an
inventory stops being saved.

Two hardenings:

* **A wedged client.** If the peer stops reading, the send blocks and the kick
  never lands. Give `Client` a `kicked_at: Option<u64>` and force-remove after
  ~2 s — but that path must run the *same* cleanup, so first extract phase 3's
  body into a `reap_client` function and call it from both. Do the extraction
  before you need it.
* **Duplicate sessions.** Nothing in the login path checks whether an account
  is already connected, so two sessions on one account are possible today and
  both write `SaveInventory` for the same key on disconnect, clobbering each
  other. That is an existing bug adjacent to this work. `/kick <name>` should
  match *every* client with that account and report the count. Making a
  duplicate login kick the older session is a good follow-up, not part of this.

---

## 9. Phasing

| # | Lands | Protocol |
|---|---|---|
| **0** | `Role`; `Record` + `MAGIC` + atomic save in `auth.rs`; `Accounts::role`/`set_role`; `ServerConfig.admins` and `SOILS_ADMINS`; `Client.role` set at login | none |
| **1** | **The one bump, v5 to v6.** Append `Command` and `UseItem` to `ClientMsg`; `Notice`, `CommandSpecs` and `Disconnect` to `ServerMsg`. Wire only `Command`/`Notice`/`CommandSpecs`. Registry, dispatch, and the cheap commands: `help list role tp spawn warp time give clear drop`. Socket size cap. Client: `ConsoleLog`, scrollback, merged `/help`, routing, delete the local `tp` | **v6** |
| **2** | `/kick`: writer-recognised `Disconnect`, `reap_client` extraction, kick deadline | — |
| **3** | `World::fill_chunk`; light-queue dedup and column ordering; `BulkEdit` + `pump_bulk_edits`; `Manifest` re-broadcast; `/fill` | none |
| **4** | Excavator in `items.yaml`; `UseItem` arm; pickup and demotion gates; `/give` minting | — |
| **5** | Teleport settle (`PendingSettles`) | none |
| **6** | `/regen` — needs off-thread regeneration and an overwrite path through `World::adopt` | none |

**Bump once, wire in stages.** Phase 1 appends all five variants but leaves
`Disconnect` and `UseItem` unhandled on both sides. Phases 2 and 4 then land
with zero protocol churn. This is the biggest scheduling win available and it
is free.

Phases 0 and 1 are each independently landable and independently useful; 3 is
the spine of 4.

**The riskiest thing is phase 0's `accounts.bin` change** — not the bulk
editor. It is the only irreversible one: it runs on every boot for every
install, and a silent mis-parse writes a corrupted map back over every account
and password on the server. The specific trap is `decode` discarding the
consumed length ([§4](#4-roles)), which makes "probe by trying the new type"
succeed on a *prefix* of the old format. The magic prefix removes the whole
class.

Runner-up is **`/fill`'s blast radius**: everything else fails loudly and
recoverably, but a mistyped fill destroys terrain that region files will
happily persist. Log every bulk edit to stdout with caller, world, box and
count. Do not build undo — chunk snapshots for a megavoxel region are megabytes
and the storage question is a project of its own.

---

## 10. Testing

Command handlers should be pure — `fn(&Caller, &Roster, &[&str]) -> Result<Vec<Effect>, String>` —
so most of the surface tests with no server at all. The integration harness is
`crates/soils-server/tests/common/mod.rs`: `TestServer::start(tag)`,
`start_with(tag, |cfg| ..)`, `start_at(dir, tag)` for restarts, and a real
tungstenite client. Add `command(line)` and `await_notice(needle)` helpers
beside the existing `edit` / `await_inventory`.

**Phase 0** — unit tests in `auth.rs`, beside the existing migration test:

* an untagged file loads as `Player` **and its passwords still verify** — the
  second half is what would actually lock people out;
* the legacy `u64` file still loads; a tagged file round-trips roles;
* `Player < Mod < Admin`, pinning the derive;
* **a corrupt file does not wipe accounts** — feed truncated bytes and assert
  `load` yields empty *without* a save having overwritten the original. This is
  the test that catches the class of bug the magic prefix exists to prevent.

**Phase 1** — registry unit tests (no server): a `Player` cannot dispatch a
`Mod` command; arity errors name the usage; unknown commands are named back;
every spec name is disjoint from `LOCAL_COMMANDS`. Protocol round-trips for
`Command` and `Notice` beside the existing block at `soils-protocol/src/lib.rs:32-98`,
plus **a test that v5 messages still encode to identical bytes** — that is what
actually proves "appending is safe" rather than assuming it.

Integration (`tests/commands.rs`): an env-listed account is Admin on login; a
promotion survives a restart via `start_at` with no env var set; `CommandSpecs`
arrives after `Init`; an overlong line is refused and not applied; and **a
server-side `/tp` moves the authoritative entity as observed by a second
client** — asserting from the teleporter's own view proves nothing, since that
is precisely the bug being fixed.

**Phase 2** (`tests/kick.rs`): a kicked client gets a reason then a closed
socket; kicking matches every session of an account; and the important one —
**a kicked client is cleaned up like any other disconnect** (a second client
sees `EntityDespawn`, and the inventory was saved). That is what catches
someone adding a second cleanup path.

**Phase 3** (`tests/bulk_edit.rs`):

* **a fill arrives as manifests, not per-voxel edits** — count `ServerMsg::Edit`
  frames during a 32³ fill and assert zero. Pins the §5.4 decision directly.
* **a large fill does not stall the tick** — measure the longest gap between
  consecutive ticks, following `stdb_auth.rs::logins_do_not_stall_the_tick`.
  `dev/server-tick.md` is explicit that total-elapsed measurements miss this
  and that any timing test here must be validated by reintroducing the bug — so
  validate it by temporarily swapping `fill_chunk` for a `World::edit` loop and
  confirming it fails.
* **a long fill does not starve the light pipeline** — fill a tall column and
  assert `world.light_settled()` (already `#[cfg(test)]`-exported at
  `world.rs:912`) becomes true within a bounded number of ticks. Without column
  ordering this never settles; this is the livelock test.
* chunks a fill touched are `Edited` for a later joiner, pinning the
  `entry.edited` invariant; over-cap volumes are refused; a fill over a chest
  closes it and reports it.

**Phase 4** (`tests/excavator.rs`): only an admin can be given one; a non-admin
holding one (obtained by having an admin `/give` then `/drop`) can neither use
nor pick it up; demotion strips it from a live inventory; use is reach-checked
and rate-limited.

Bulk-edit tests are the slowest and `TestServer::start` serializes behind
`SERVER_GATE`, so keep them in their own file rather than extending the other
files' message deadlines.

---

## 11. Risks

* **The `accounts.bin` migration is the one that can destroy data**, and the
  mechanism is subtle: `decode` discards the consumed length, so a
  fallback-decode probe can succeed on a prefix. Magic prefix, atomic write,
  and the corrupt-file test are all one mitigation and should land together.
* **`/fill` has no undo** and region files persist it. Cap it, log it, and
  consider requiring a confirmation for the first fill of a session.
* **The light pipeline can livelock** under a multi-tick fill that keeps
  re-dirtying a column in flight. Column ordering is not an optimization here;
  it is correctness.
* **Two sessions per account are already possible** and already clobber each
  other's saved inventory. `/kick` makes this visible; it does not cause it.
* **The command set is a permanent surface.** Every command a player types is
  one that has to keep working, and `min_role` in one table is what keeps that
  reviewable in one place.
