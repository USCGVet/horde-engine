//! Engine basics every crate and game builds on: stateless randomness, per-phase step timing, and the
//! [`Simulation`] interface the runtime drives.

pub mod rng;
mod timing;

pub use timing::{MAX_PHASES, PhaseTimer, Phases};

use horde_jobs::JobPool;

/// A game's world, stepped by the runtime at a fixed rate.
///
/// Implementations must keep `step` deterministic: the same state and inputs give a bit-identical result
/// whatever the pool's thread count (see docs/DESIGN.md, "Determinism contract").
pub trait Simulation {
    /// Ticks per second.
    fn tick_rate(&self) -> u32;

    /// Advances the world by one tick and reports how long each phase took.
    fn step(&mut self, pool: &JobPool) -> Phases;

    /// A short game-specific status line for the runtime's per-second report.
    fn status(&self) -> String;

    /// A hash of the whole world state, for determinism checks.
    fn checksum(&self) -> u64;
}
