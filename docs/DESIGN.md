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
| `horde-runtime` | Runs a `Simulation` as a dedicated server: CPU placement, tick loop, reports | M1 |
| `horde-net` | Protocol, snapshots, interest management, lag compensation | M2 |
| `horde-client` | Renderer, input, prediction, interpolation | M2 |
| `horde-assets` | glTF pipeline, terrain | M3 |

Planned engine systems that games build on: physics and vehicles, destructible objects, navigation and
AI building blocks, spawning, data-driven definitions.

### Games (`games/`)

| Game | What it is |
|---|---|
| `skirmish` | Test game: a small defending side holds capture points against a horde. Library plus `skirmish-server`. |

A game implements `horde_core::Simulation` (tick rate, `step`, status line, checksum) and owns its
components, rules and content. Its server binary parses its own options, then hands the rest to
`horde_runtime`.

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

## Simulation model

- Fixed rate, 30 Hz by default (the same as BF2's server).
- Entities stored as structure-of-arrays: one `Vec` per field. An entity is an index.
- A step is a sequence of phases, timed with `PhaseTimer`. Skirmish's step, for example:
  1. **Grid** (serial): a counting sort of living soldiers into the spatial grid, one group per team,
     with copies of the positions that queries read.
  2. **Think** (parallel, chunks of 64): sensing, targeting, movement and firing. Each task writes only
     its own soldiers' fields and its own chunk's hit buffer, and reads only the grid snapshot.
  3. **Resolve** (serial): damage applied in chunk order, then deaths, respawns and capture points.
- Randomness comes from a hash of (seed, tick, entity, purpose), never from shared RNG state.

**Determinism contract for parallel passes:** a task may write only to indices it owns, plus per-chunk
buffers. Anything touching another entity goes through those buffers and is applied serially in chunk
order. Floating-point operations per entity are identical however many threads run, so results are
bit-identical. Every game's tests must check this with different thread counts, as skirmish's do.

## Networking (M2)

- UDP, server-authoritative, with versioned packets.
- Snapshots are delta-compressed against the last state each client acknowledged.
- **Interest management.** Each client gets a per-entity priority based on distance, visibility, whether
  the entity is shooting at them, and time since its last update. Nearby soldiers update every tick and
  distant ones less often. This replaces BF2's fixed budget that starved close-range updates.
- **Client prediction** for the local player. Remote soldiers interpolate behind a ~100 ms buffer.
- **Lag compensation.** The server keeps a short position history and checks each shot against what that
  client was seeing when it fired.
- 32-bit entity IDs with a generation counter, and no fixed object ceiling.

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

- `cargo test` covers the CPU plan, the job pool, the spatial grid (against brute force) and skirmish's
  determinism.
- `cargo run --release -p skirmish --bin skirmish-server -- --bench --seconds 10` runs ticks back to back
  and reports per-phase time and CPU use. Run it before and after any performance change. Build first:
  `cargo test` doesn't rebuild the server binary.

## Milestones

1. **M1 — Headless simulation benchmark (done).** CPU plan, job pool, deterministic sim; 1,000+ bots
   holding 30 Hz on the efficient cores. Engine and first game split apart.
2. **M2 — Networking and a minimal client.** Join over LAN, see and fight the horde, with smooth
   movement at 1,000 bots.
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

## Measurements

Milestone 1, i9-14900KF, shared placement (tick thread plus 16 E-core workers = 17 threads), 1,024 m
map, 14 defenders against the rest.

| Bots | Back to back: mean tick | Headroom at 30 Hz | Real time: mean / p99 tick |
|---|---|---|---|
| 1,000 | 0.025 ms | 1,339× | 0.56 / 5.0 ms |
| 10,000 | 0.16 ms | 210× | 0.76 / 5.2 ms |
| 50,000 | 0.75 ms | 45× | — |
| 100,000 | 1.6 ms | 21× | — |
| 200,000 | 3.0 ms | 11× | — |

In real time the server averages well under one core of CPU.

**Open issue — wake-up latency.** At 30 Hz the workers sleep between ticks, and waking the efficient
cores costs up to about 5 ms on some ticks. That's why real-time p99 is about 5 ms while back to back
it's 0.03–0.2 ms. It fits the 33 ms budget, so it stays for now. If tick latency starts to matter
(networking, higher tick rates), the fix is to wake the pool about 1 ms before each tick: a few percent
of each E-core's time spent spinning in exchange for a flat p99.
