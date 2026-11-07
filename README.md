# Horde Engine

A Rust game engine for battles with huge numbers of AI soldiers: a small human side against hundreds to
thousands of bots, single-player or LAN, on one 64-bit, fully multi-threaded host. Games built on it live
in `games/`.

The foundations, the engine/game boundary and the reasons behind each choice are in
[docs/DESIGN.md](docs/DESIGN.md).

## Layout

- `crates/horde-cpu`: CPU layout and the plan for which cores the server uses
- `crates/horde-jobs`: job pool for parallel passes
- `crates/horde-core`: randomness, phase timing, the `Simulation` trait
- `crates/horde-spatial`: spatial grid for neighbour queries
- `crates/horde-runtime`: runs a `Simulation` as a dedicated server
- `crates/horde-net`: UDP protocol and prioritised replication; `horde-netprobe` test client
- `crates/horde-client`: window, DirectX 12 renderer, input, camera, smoothing
- `games/skirmish`: test game, a small defending side against a horde, and its server
- `games/skirmish-client`: the skirmish client
- `tools/`: dependency license check, client screenshot

## Status: milestone 2b, playable

The server leaves the performance cores to the client on the same PC, runs skirmish's deterministic
battle on a pinned job pool at a fixed tick rate, and accepts players over UDP. You take over one of the
defenders with a rifle; the horde charges and attacks up close, a little slower than you can run. Every soldier's state reaches you, nearby ones every tick and distant ones as the bandwidth
budget allows, and the client smooths them between updates. Every shot near you shows as a tracer, and
your hits and kills show as markers around the crosshair. Placeholder graphics for now: soldiers are
coloured blocks (yours gold, your side blue, the horde red) on a grid.

Controls: click to capture the mouse, mouse aims, WASD moves, left button fires, V switches first/third
person, mouse wheel zooms, Esc frees the mouse.

## Build and run

Needs a stable Rust toolchain (pinned in `rust-toolchain.toml`).

```
cargo test
cargo build --release
cargo run --release -p skirmish --bin skirmish-server
cargo run --release -p skirmish --bin skirmish-server -- --bots 5000 --bench --seconds 10
cargo run --release -p skirmish --bin skirmish-server -- --help

# play: start the server, then the client in another terminal
cargo run --release -p skirmish --bin skirmish-server -- --listen 127.0.0.1:7777 --seconds 0
cargo run --release -p skirmish-client

# or join with headless test players that measure update rates
cargo run --release -p horde-net --bin horde-netprobe -- --server 127.0.0.1:7777 --clients 4
```

Third-party crates must be under permissive licenses; check with `pwsh tools/check-licenses.ps1`.

Binaries are built with `target-cpu=native` for the PC they're built on (`.cargo/config.toml`), so they
may not run on an older CPU.

## License

Proprietary, all rights reserved. See [LICENSE](LICENSE).
