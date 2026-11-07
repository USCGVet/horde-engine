//! Networking: a small UDP protocol, server-to-client replication of every entity's state, and the server
//! and client endpoints.
//!
//! Replication is built for crowds:
//!
//! - **Only changes are sent.** The server tracks what each client has acknowledged for every entity. An
//!   entity whose quantised state matches that baseline costs nothing.
//! - **Priority, not round-robin.** Each changed entity's priority grows every tick by its relevance to
//!   the client (closer means faster). The highest priorities fill the client's per-tick byte budget and
//!   reset. So nearby soldiers update every tick, distant ones less often, and nothing starves.
//! - **No small caps.** Entity IDs are 32-bit varints; a snapshot spans as many packets as the budget
//!   allows.
//! - **Shots ride along.** Shots fired near a client, and all of its own, go out once in the snapshot of
//!   the tick they were fired, for tracers and hit markers.
//!
//! See docs/DESIGN.md, "Networking".

mod ack;
mod client;
mod server;
mod sockets;
mod wire;

pub use client::{NetClient, ReceivedShot, RemoteEntity};
pub use server::{NetServer, ServerConfig};
pub use wire::Quantized;

/// What a game replicates for every entity. Games map their own state onto it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EntityState {
    /// Position on the ground plane, in metres from the world's corner.
    pub x: f32,
    pub z: f32,
    /// Facing, in radians; 0 faces +z and positive turns toward +x.
    pub yaw: f32,
    pub team: u8,
    /// Game-defined type of entity (0–31).
    pub kind: u8,
    pub alive: bool,
    /// 0 to 1.
    pub health: f32,
}

/// A shot fired during the last step, for clients to show (tracers, hit markers). Engine-defined like
/// [`EntityState`]: every shooter-style game fires shots.
///
/// Shots aren't stored or resent. A client gets each relevant one once, in the snapshot of the tick it was
/// fired, and loses it if that packet is lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shot {
    pub shooter: u32,
    /// The entity aimed at, or [`Shot::NO_TARGET`] for a shot at nothing in particular, which flies along
    /// the shooter's facing.
    pub target: u32,
    pub hit: bool,
    /// The hit killed the target.
    pub kill: bool,
}

impl Shot {
    pub const NO_TARGET: u32 = u32::MAX;
}

/// One frame of a player's controls, as sent by a client.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerInput {
    /// Desired movement in world space, each -1 to 1.
    pub move_x: f32,
    pub move_z: f32,
    /// Aim direction, in radians (see [`EntityState::yaw`]).
    pub yaw: f32,
    pub fire: bool,
}

/// What the network server needs from a game.
pub trait Replicated {
    /// Side length of the square world, in metres. Positions are quantised against it.
    fn world_size(&self) -> f32;

    fn entity_count(&self) -> usize;

    fn entity_state(&self, entity: usize) -> EntityState;

    /// Gives a newly connected player an entity to control, or `None` if there's no room.
    fn spawn_player(&mut self) -> Option<u32>;

    /// Hands a disconnected player's entity back to the game.
    fn despawn_player(&mut self, entity: u32);

    /// Sets the controls a player's entity follows on the next step.
    fn apply_input(&mut self, entity: u32, input: &PlayerInput);

    /// Shots fired during the last step. Games without shooting can leave this empty.
    fn shots(&self) -> &[Shot] {
        &[]
    }
}
