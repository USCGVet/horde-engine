# Horde Engine

A Rust game engine for battles with huge bot counts, plus the games built on it. Read `docs/DESIGN.md`
first: it holds the foundations, the engine/game boundary, the milestone plan and the decision log.

## Layout

- `crates/` holds engine crates (`horde-*`). They must not contain any one game's rules, units or numbers.
- `games/` holds games. Each implements `horde_core::Simulation` and owns its rules and content.
- If a second game would want it, it's engine. If it encodes one game's design, it's game.

## Commands

- `cargo test` covers the CPU plan, the job pool, the spatial grid, skirmish's determinism, the wire format,
  shot delivery over loopback, and the client's smoothing, camera and shot effects.
- `cargo clippy --all-targets` must stay warning-free.
- `cargo build --release`, then `cargo run --release -p skirmish --bin skirmish-server -- --bench --seconds 10`,
  is the headless benchmark. Run it before and after any performance change and quote both numbers.
  Build first: `cargo test` doesn't rebuild the server binary, so a stale one gives misleading numbers.
- Network check: run `skirmish-server --listen 127.0.0.1:7777` in the background, then
  `horde-netprobe --server 127.0.0.1:7777 --clients 4 --seconds 15` (both from `target/release`). Update
  intervals near the player should stay at 1 tick.

## Rules

- **Parallel passes:** a task writes only the entity indices in its own range and its own chunk's
  buffers. Effects on other entities go through per-chunk buffers applied serially in chunk order. This
  keeps results identical for any thread count, and every game's determinism test must keep passing.
- **Hot loops:** no allocation inside per-tick loops. Reuse buffers (clear, don't reallocate).
- **Randomness:** only from `horde_core::rng` over (seed, tick, entity, salt), never shared state.
- **Unsafe:** every `unsafe` block gets a `// SAFETY:` comment.
- **Dependencies:** the simulation core (`horde-cpu`, `horde-jobs`, `horde-core`, `horde-spatial`,
  `horde-runtime`, `horde-net`, game simulation crates) stays dependency-free. Any crate added anywhere must
  be under a permissive license; run `pwsh tools/check-licenses.ps1` after changing dependencies, and
  record the decision in the decision log.
- **Coordinates:** left-handed, x right, y up, z forward; yaw 0 faces +z and positive yaw turns right
  (toward +x). Use glam's `camera::lh` functions, never `rh`.
- **Client screenshots:** `pwsh tools/screenshot-window.ps1 skirmish-client out.png` captures the client
  window, including GPU content, at full resolution.
- **Content:** everything is original, made or generated for this project. Never add third-party game
  assets or data.
- **License:** proprietary, all rights reserved (`LICENSE`). Don't add open-source license headers or files.
- **Decisions:** record new architectural decisions in the `docs/DESIGN.md` decision log, with the reason.

## Machine

The dev PC has an i9-14900KF: 8 hyperthreaded performance cores (CPUs 0–15) and 16 efficient cores
(CPUs 16–31). In shared mode the server's tick thread is on CPU 14 and its workers on CPUs 16–31.
