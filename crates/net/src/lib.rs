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
//!
//! See docs/DESIGN.md, "Networking".

mod ack;
mod client;
mod server;
mod sockets;
mod wire;

pub use client::{NetClient, RemoteEntity};
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
}
