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
- `games/skirmish`: test game, a small defending side against a horde

## Status: milestone 1

A headless simulation benchmark. The server leaves the performance cores to a game client on the same
PC, runs skirmish's deterministic battle on a pinned job pool at a fixed tick rate, and reports per-phase
tick time and CPU use. Networking and a client come in milestone 2.

## Build and run

Needs a stable Rust toolchain (pinned in `rust-toolchain.toml`).

```
cargo test
cargo build --release
cargo run --release -p skirmish --bin skirmish-server
cargo run --release -p skirmish --bin skirmish-server -- --bots 5000 --bench --seconds 10
cargo run --release -p skirmish --bin skirmish-server -- --help
```

Binaries are built with `target-cpu=native` for the PC they're built on (`.cargo/config.toml`), so they
may not run on an older CPU.

## License

Proprietary, all rights reserved. See [LICENSE](LICENSE).
