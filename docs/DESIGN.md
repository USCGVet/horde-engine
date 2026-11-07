# Horde Engine — design

A game engine for battles with huge numbers of AI soldiers: a small human side against hundreds to
thousands of bots, single-player or LAN multiplayer, on one 64-bit, fully multi-threaded host. The first
target is a BF2-style combined-arms shooter, but the engine is meant to carry many games. Each game
lives in `games/` and supplies its own rules and content.

This document holds the foundations that are expensive to change later. Each later milestone adds to it
instead of redefining it. Decisions and their reasons live in the [decision log](#decision-log).

## Why this exists

Battlefield 2 with the AIX 2.0 mod could run about 200 bots, but its 2005 engine stopped there:

- Each bot's position reached clients too rarely, so bots rubber-banded or paused mid-walk.
- Networked objects were capped at 1,024 IDs, and magazines at 1,022 rounds.
- All bot AI ran on one core while the server used about 7% of it, so the hardware was never the limit.

Every one of those is an engine design choice. This engine makes different ones. BF2 is the inspiration
only: no BF2 code, data or assets are used.

## Principles

1. **Server first.** The simulation is a plain library with no graphics; the dedicated server is that
   library on a fixed-rate loop. The client is a view onto it.
2. **Measured, not guessed.** Every system reports its time per tick. Performance changes come with
   before/after numbers from the headless benchmark.
3. **Deterministic steps.** Given the same seed and inputs, a step produces bit-identical results no
   matter how many threads run it or how they're scheduled. That makes replays, regression tests and
   multithreaded debugging possible.
4. **No fixed small caps.** Entity IDs, counts and network fields are sized for the largest battle we
   intend, not for 2005 bandwidth.
5. **Data-driven content.** Weapons, kits, vehicles and maps are defined in text files, so games stay
   moddable.
6. **Replaceable rendering.** Game logic never depends on the renderer; rendering sits behind a thin
   interface and can be swapped.
7. **Original content only.** Every model, texture, sound, map and line of code is made or generated for
   this project. No third-party game assets, ever. Standard formats (glTF) come in through the asset
   pipeline.
8. **Engine and game stay separate.** Engine crates know nothing about any particular game. Soldiers,
   damage numbers, tickets and capture points belong to a game.

## Architecture

### Engine (`crates/`)

| Crate | Role | Status |
|---|---|---|
| `horde-cpu` | CPU topology (P-cores vs E-cores), the CPU plan, thread pinning, process CPU time | M1 |
| `horde-jobs` | Fork-join job pool tuned for fixed-rate ticks; `DisjointSlice` for parallel writes | M1 |
| `horde-core` | Stateless randomness, per-phase step timing, the `Simulation` trait | M1 |
| `horde-spatial` | Uniform grid bucketed by (group, cell) for neighbour queries | M1 |
| `horde-runtime` | Runs a `Simulation` as a dedicated server: CPU placement, tick loop, `TickHooks`, reports | M1 |
| `horde-net` | UDP protocol, prioritised replication, server and client endpoints, `horde-netprobe` | M2a |
| `horde-client` | Window, DirectX 12 renderer (wgpu), input, camera, interpolation | M2b |
| `horde-assets` | glTF pipeline, terrain | M3 |

Planned engine systems that games build on: physics and vehicles, destructible objects, navigation and
AI building blocks, spawning, data-driven definitions.

### Games (`games/`)

| Game | What it is |
|---|---|
| `skirmish` | Test game: a small defending side holds capture points against a horde. Library plus `skirmish-server`. |
| `skirmish-client` | Launches `horde-client` against a skirmish server. |

A game implements `horde_core::Simulation` (tick rate, `step`, status line, checksum) and owns its
components, rules and content. To be playable over the network it also implements
`horde_net::Replicated`: it maps its entities onto the engine's `EntityState` and takes players' inputs.
Its server binary parses its own options, then hands the rest to `horde_runtime`, passing a `NetServer`
as the runtime's `TickHooks` when it listens for players.

### Where a feature belongs

- **Engine:** the mechanism — how things are stored, scheduled, queried, simulated, networked and drawn.
  Spatial queries, physics, the vehicle seat-and-entry framework, destructible-object state, spawn
  points, the job pool.
- **Game:** the rules and numbers — which soldiers and vehicles exist, how much damage a shot does, how
  tickets and capture points work.

If a second game would want it, it's engine. If it encodes one game's design, it's game.

## CPU plan

The server shares the PC with a game client, so it doesn't take every core. `horde-cpu` detects the
core layout and builds a `CpuPlan`:

- **Shared with a client (default).** On hybrid CPUs the client keeps the performance cores, since games
  run their main and render threads there. The server takes:
  - its tick thread on the last performance core, at slightly raised priority, and
  - one simulation worker pinned to each efficient core.

  On this PC (i9-14900KF: 8 P-cores = CPUs 0–15, 16 E-cores = CPUs 16–31) that means the tick thread on
  CPU 14, workers on CPUs 16–31, and CPUs 0–13 and 15 left to the client. On a uniform CPU the server
  takes the upper half of the cores.
- **Dedicated.** One thread on every physical core.

Workers sit on the first hyperthread of each core only: the simulation is compute-bound, so a second
thread per core adds little throughput and makes tick times noisier.

Between ticks the server should cost the client nothing. Workers spin for at most 250 µs after a parallel
pass (the next pass in the same tick usually starts within microseconds), then sleep until the next
pass. The tick thread sleeps until the next tick deadline.

Every simulation system shares one tick budget and one worker pool. Cosmetic work (debris, particles,
decals, animation detail) runs on the client, never on the server's workers.

## Coordinates

The world is left-handed: x right, y up, z forward. Positions on the ground are (x, z) in metres from
the world's corner. Yaw is in radians: 0 faces +z, and positive yaw turns toward +x, which is turning
right. Facing +z, +x is on the right of the screen. The client's camera uses left-handed matrices to
match; right-handed ones mirror the view.

## Simulation model

- Fixed rate, 30 Hz by default (the same as BF2's server).
- Entities stored as structure-of-arrays: one `Vec` per field. An entity is an index.
- A step is a sequence of phases, timed with `PhaseTimer`. Skirmish's step, for example:
  1. **Grid** (serial): counting sorts of living soldiers into two spatial grids, one group per team,
     with copies of the positions that queries read. 32 m cells for finding targets, 8 m cells for
     crowding.
  2. **Think** (parallel, chunks of 64): sensing, targeting, movement and firing. Each task writes only
     its own soldiers' fields and its own chunk's hit buffer, and reads only the grid snapshots.
  3. **Resolve** (serial): damage applied in chunk order, then deaths, respawns and capture points.
- Randomness comes from a hash of (seed, tick, entity, purpose), never from shared RNG state.
- **Combat** (skirmish): defenders carry rifles. The horde attacks by touch by default: it charges the
  nearest defender in sight at 4 m/s, slower than a running soldier's 5 m/s so you can get away, and
  strikes for 20 damage a second once within 1.5 m (`--horde-attack shoot` gives it rifles instead).
  A player's shot hits the first enemy along the line of aim within 60 m, if its body is within 0.75 m of
  the line plus 5 cm per metre of distance; 9 in 10 such shots hit.
- **Crowds** (skirmish): soldiers spawn across a wide base area or a 20 m disc around a held point, and
  each heads for its own spot inside an objective rather than its centre. Teammates closer than 1.2 m
  push each other apart, up to 4 m/s. Each soldier reacts to at most 6 such neighbours, which bounds the
  cost in a packed crowd. Two soldiers on exactly the same spot split in a direction hashed from their
  pair, so the result stays deterministic.

**Determinism contract for parallel passes:** a task may write only to indices it owns, plus per-chunk
buffers. Anything touching another entity goes through those buffers and is applied serially in chunk
order. Floating-point operations per entity are identical however many threads run, so results are
bit-identical. Every game's tests must check this with different thread counts, as skirmish's do.

## Networking (M2)

Done in M2a (`horde-net`):

- **UDP, server-authoritative, versioned packets.** Every packet carries a header with magic, version,
  kind, its own sequence number, and an ack plus a 32-bit field of earlier acks. Datagrams stay under
  1,200 bytes.
- **Only changes are sent.** For every client and every entity, the server keeps the quantised state the
  client has acknowledged (its baseline). An entity whose current state matches its baseline costs
  nothing. When a snapshot packet is acknowledged, the states in it become the new baselines; lost
  packets just leave the entity eligible again.
- **Interest management by priority accumulation.** Each changed entity's priority grows every tick by
  its relevance to that client: 1 within 60 m, falling with distance squared beyond, never below 0.01. The
  highest priorities fill the client's per-tick byte budget (default 48 KB) and reset to 0. Nearby
  soldiers update every tick and distant ones as often as the budget allows, and nothing starves. This
  replaces BF2's fixed budget that starved close-range updates.
- **Compact records.** Entity id as a varint delta in ascending order (usually 1 byte), then x and z as
  16-bit fractions of the world (1.6 cm on a 1 km map), facing in 1/256 turns, a flags byte (alive, team,
  kind) and health: 8 bytes per entity update.
- **Inputs** go client to server each frame, with the previous 3 repeated against packet loss. The
  server applies the newest one each tick.
- **Shots** (protocol version 2) ride in the snapshot of the tick they were fired: shooter, target (or
  none) and whether it hit and killed, usually 5 bytes. A client gets its own shots, shots at it, and
  shots whose shooter or target is within 250 m, using at most a quarter of its budget. They're sent once
  and never resent: a lost tracer doesn't matter, and nothing waits on them.
- 32-bit entity ids and multi-packet snapshots: no fixed object ceiling.

Still to do:

- **Client prediction** for the local player, so your own movement responds without the one-tick delay.
- **Lag compensation.** The server keeps a short position history and checks each shot against what that
  client was seeing when it fired. Until then, player shots are checked against current positions, so
  with the client's interpolation delay you need to lead moving targets slightly.
- Building snapshots for several clients in parallel on the job pool.

## Client (M2b)

- **Rendering:** DirectX 12 through wgpu. One draw for the ground, one instanced draw for every soldier.
  Vsync on, so the client never renders frames the display can't show. The client process keeps to the
  CPU plan's client cores, staying off the server's.
- **Smoothing:** a client clock tracks the server tick, smoothed against arrival jitter. Other soldiers
  are drawn 3 ticks (100 ms) in the past and blended between the samples around that time. That's enough
  for a soldier updated only every other tick to always have a later sample. Past the newest sample they
  extrapolate for at most 2 ticks. A jump over 15 m (a respawn) snaps instead of sliding. Your own
  soldier is drawn only 1 tick behind.
- **Shots:** every shot is a glowing tracer streak flying at 350 m/s, slower than a bullet so the eye can
  follow it. Other soldiers' shots wait until the drawn battle reaches the tick they were fired, then fly
  between the soldiers as drawn, so a kill lands just as its target drops; a miss passes beside its target.
  Your own tracers leave the moment you fire, at the server's rate rounded to whole ticks, so firing feels
  immediate. The server's verdict on your shots shows as a white hit marker or a red kill marker around
  the crosshair, and losing health flashes the screen edges red.
- **Reconnecting:** after 3 seconds without a packet the client treats the server as gone and starts
  connecting again. Each accepted connection is a new session, and the client drops everything it kept
  from the previous one, so a restarted server just reappears.
- **Camera:** first or third person, switched with V. The mode and third-person zoom are remembered per
  entity kind, so when vehicles arrive a tank and a soldier each keep the view the player prefers. In
  first person your own soldier isn't drawn.
- **Controls:** click to capture the mouse, mouse aims, WASD moves relative to where you look, left button
  fires, V switches view, mouse wheel zooms in third person, Esc frees the mouse.

## AI (M4)

- Layers: commander (which objectives), squads (formation and routes), soldiers (cover, targets, firing).
- Navigation meshes built offline per map, as tiles. Path queries are batched and run in parallel.
- Level of detail by distance to humans: full detail nearby, cheaper thinking far away. That's a real
  per-tick budget in milliseconds, not round-robin starvation.

## Content and rendering (M2–M3)

- All content is original: made by hand or generated for this project.
- Models, animations and maps come in as glTF and are converted to engine-native, memory-mappable files.
- Rendering on wgpu (Vulkan/DX12) behind a thin interface. Crowds are drawn with GPU instancing, and
  distant soldiers fall back to cheap impostors.

## Testing and benchmarks

- `cargo test` covers the CPU plan, the job pool, the spatial grid (against brute force), skirmish's
  determinism and shot reporting, the wire format, shot delivery from server to client over loopback,
  client smoothing, the camera and the shot effects.
- `cargo run --release -p skirmish --bin skirmish-server -- --bench --seconds 10` runs ticks back to back
  and reports per-phase time and CPU use. Run it before and after any performance change. Build first:
  `cargo test` doesn't rebuild the server binary.
- Network check: start `skirmish-server --listen 127.0.0.1:7777`, then run
  `horde-netprobe --server 127.0.0.1:7777 --clients 4`. It joins as players, moves and fires, and reports
  bandwidth plus how often entities near, mid-range and far from the player get updated.

## Milestones

1. **M1 — Headless simulation benchmark (done).** CPU plan, job pool, deterministic sim; 1,000+ bots
   holding 30 Hz on the efficient cores. Engine and first game split apart.
2. **M2 — Networking and a minimal client.** Join over LAN, see and fight the horde, with smooth
   movement at 1,000 bots.
   - **M2a — Networking (done).** Protocol, prioritised replication, player control, headless probe.
   - **M2b — Client (playable).** Window, renderer, input, interpolation, first/third-person camera.
     Prediction and lag compensation still to do.
3. **M3 — Terrain.** Heightmap terrain from original or generated data.
4. **M4 — Navigation-mesh AI with squads and a commander.**
5. **M5 — Vehicles.**
6. **M6 — Data-driven weapons, kits and maps; moddability.**

## Decision log

| # | Decision | Why |
|---|---|---|
| D1 | Rust, stable toolchain, edition 2024 | Compiler-enforced thread safety for a heavily parallel engine; safe packet parsing; refactors stay safe as the codebase grows over years. |
| D2 | No third-party crates in the M1 core | Full control of the hot path and thread placement. Crates come in where they clearly pay off (networking, rendering). |
| D3 | Own job pool instead of rayon | Needs per-thread core pinning, spin-then-sleep tuned to a fixed tick, and the caller taking part in every pass. A pool this narrow is about 200 lines. |
| D4 | Fixed 64-entity chunks with per-chunk output buffers | Deterministic results independent of thread count. Chunks are small enough to balance 17 threads. |
| D5 | `target-cpu=native`, fat LTO, one codegen unit | Built and run on the same PC, so use every instruction it has. Binaries won't run on older CPUs. |
| D6 | `panic = "abort"` | A panic inside a parallel pass can't leave the world half-updated. |
| D7 | Spatial grid bucketed by (group, cell) | A query for one group touches only that group. With one shared bucket per cell, horde soldiers scanned their own crowd: 50,000 bots took 671 ms per tick single-threaded. Per-team buckets cut that to 2.9 ms. |
| D8 | Engine crates in `crates/`, games in `games/`, joined by the `Simulation` trait | Many games can share one engine, and game rules can't leak into engine systems. Split at M1, before networking and vehicles were written against a mixed crate. |
| D9 | Original content only; proprietary license | The author owns everything in the repository. See `LICENSE`. |
| D10 | Own UDP protocol on `std::net`, no networking crate | The hard part is replication policy (baselines, priorities, budgets), which no general crate does our way. The transport underneath is about 150 lines. |
| D11 | Engine-defined `EntityState` (position, facing, team, kind, alive, health) that games map onto | One compact 8-byte record serves every shooter-style game. A game-defined payload can be added when one needs it. |
| D12 | Replication by acknowledged baselines plus priority accumulation within a byte budget | Stationary entities cost nothing, nearby ones update every tick, and distant ones degrade gracefully instead of starving. Measured below. |
| D13 | Networking runs as `TickHooks` around the step, on the tick thread | Keeps the runtime independent of networking and inputs deterministic (applied before the step). Per-client snapshot building can move onto the job pool when player counts call for it. |
| D14 | Third-party crates only under permissive licenses (MIT, Apache-2.0, BSD, Zlib, ISC and similar), checked by `tools/check-licenses.ps1`; the simulation core stays dependency-free | Lets a proprietary engine use them without publishing its source. Copyleft (GPL, LGPL, AGPL, MPL) is out. The check runs over every crate in the lock file. |
| D15 | Client on wgpu 30 (DirectX 12 only), winit, glam, bytemuck, pollster | wgpu is a thin layer over DirectX 12 used in production by Firefox. Instancing keeps its per-draw overhead to a couple of draws a frame. Other backends are compiled out (233 instead of 256 crates). Replaceable behind `horde-client`'s renderer if measurements ever say so. |
| D16 | Left-handed world coordinates (see Coordinates) | Matches the simulation's yaw convention, where turning toward +x is turning right. |
| D17 | Crowd separation from a second, fine grid (8 m cells), capped at 6 neighbours per soldier | Without it soldiers stacked on one spot at spawns and objectives. The 32 m targeting grid would make each separation query scan hundreds of teammates. The cap keeps a packed horde from costing quadratically. Raised the 200,000-bot tick from 3.0 to 9.4 ms (12.3 ms uncapped) and the 1,000-bot tick from 0.025 to 0.10 ms; both still far inside the budget. |
| D18 | Spawn areas and per-soldier objective spots instead of single points | Separation alone can't untangle hundreds of soldiers placed on one coordinate in the same tick. Spreading them at the source is free. |
| D19 | The client reconnects by itself after 3 seconds of server silence | A restarted server should just come back, without restarting the client. Sessions are counted so the client can drop the old game's state. |
| D20 | Engine-defined `Shot` events (shooter, target, hit, kill), sent once and unreliably in snapshots, filtered by distance | Every shooter game needs tracers and hit confirmation. A shot is an event, not state, so it has no baseline and isn't resent. Naming the target instead of an end point keeps it small and lets the client draw the tracer to where it shows the target. |
| D21 | The player's own tracers are drawn locally the moment they fire; hit markers wait for the server | Instant feedback without letting the client decide hits. Until lag compensation, a local tracer can cross an enemy the server says was missed. |
| D22 | Skirmish's horde attacks by touch by default, and moves slower than a running soldier | With rifles, 986 soldiers cut a player down the moment they came into range. A horde that has to close in gives the player something to fight and somewhere to run. Over about 6 simulated minutes the defenders kill twice as many as they lose, but the horde still takes every point. |
| D23 | A player's shot hits the first enemy along the line of aim, not the nearest enemy | Checking only the nearest enemy meant aiming at one soldier in a crowd missed whenever another was closer. |

## Measurements

Simulation, i9-14900KF, shared placement (tick thread plus 16 E-core workers = 17 threads), 1,024 m
map, 14 defenders against the rest, with crowd separation (D17). Back to back for 10 seconds of wall
time, which covers the march and the fighting.

| Bots | Back to back: mean / p99 tick | Headroom at 30 Hz | Real time: mean / p99 tick |
|---|---|---|---|
| 1,000 | 0.092 / 0.095 ms | 361× | 0.57 / 5.1 ms |
| 10,000 | 0.46 / 0.62 ms | 72× | 0.86 / 5.1 ms |
| 50,000 | 2.3 / 6.7 ms | 15× | — |
| 100,000 | 4.6 / 8.4 ms | 7× | — |
| 200,000 | 9.6 / 13.6 ms | 3.5× | — |

In real time the server averages about a third of one core. The real-time p99 of about 5 ms is the
workers waking from sleep between ticks, not work.

Before crowd separation (M1), the 200,000-bot tick was 3.0 ms and the 1,000-bot tick 0.025 ms. Those
were averaged over only the first 10 simulated seconds, before the battle: until this change the
benchmark summary kept only `seconds × tick rate` tick times. At 200,000 bots the grid phase is 2.0 ms
(two serial rebuilds) and thinking 7.6 ms.

Networking (M2a), same PC over loopback, players measured with `horde-netprobe`. An update interval of 1
tick means every tick (33 ms):

| Bots | Players | Out per player | Server net time | Update interval near / mid / far (p50 = p99) |
|---|---|---|---|---|
| 1,000 | 1 | 237 KB/s | 0.08 ms/tick | 1 / – / 1 tick |
| 10,000 | 4 | 1.3 MB/s | 1.6 ms/tick (all 4) | 1 / 1 / 2 ticks |

At 10,000 bots the 48 KB budget fits about 5,400 updates per tick, so the 9,986 distant horde soldiers
each update every other tick while everything within 200 m updates every tick.

Adding shots (D20) left both unchanged: recording them costs no measurable step time (1,000 bots 0.093
ms, 10,000 bots 0.456 ms, 200,000 bots 9.7 ms), and with 4 players every update interval stayed at 1 tick.

**Open issue — wake-up latency.** At 30 Hz the workers sleep between ticks, and waking the efficient
cores costs up to about 5 ms on some ticks. That's why real-time p99 is about 5 ms while back to back
it's 0.03–0.2 ms. It fits the 33 ms budget, so it stays for now. If tick latency starts to matter
(networking, higher tick rates), the fix is to wake the pool about 1 ms before each tick: a few percent
of each E-core's time spent spinning in exchange for a flat p99.
