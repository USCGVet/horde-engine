//! Skirmish: the engine's first test game. A small defending side holds capture points against a horde.
//!
//! Soldiers are stored as structure-of-arrays. A step is a serial grid rebuild, one parallel pass over all
//! soldiers in fixed 64-soldier chunks, and a serial resolve. A task in the parallel pass writes only its
//! own soldiers and its own chunk's shot buffer; damage to other soldiers is applied afterwards in chunk
//! order, so the result never depends on how many threads ran it (docs/DESIGN.md, "Determinism contract").

use horde_core::{PhaseTimer, Phases, Simulation, rng};
use horde_jobs::{DisjointSlice, JobPool};
use horde_net::{EntityState, PlayerInput, Replicated, Shot};
use horde_spatial::Grid;

pub const TEAMS: usize = 2;
/// `CapturePoint::owner` when neither team holds the point.
pub const NEUTRAL: u8 = 2;
const NO_TARGET: u32 = Shot::NO_TARGET;
/// Soldiers per parallel chunk: large enough to amortise claiming a chunk, small enough to balance
/// 17 threads at 1,000 soldiers.
const CHUNK: usize = 64;
/// Cells for sighting enemies (searches reach 90 m).
const GRID_CELL: f32 = 32.0;
/// Cells for crowd separation (searches reach about a metre), so a dense crowd isn't scanned whole.
const CROWD_CELL: f32 = 8.0;

// Combat tuning: metres, metres per second, seconds.
const SIGHT: f32 = 90.0;
pub const FIRE_RANGE: f32 = 60.0;
const RUN_SPEED: f32 = 5.0;
const COMBAT_SPEED: f32 = 1.5;
const FIRE_INTERVAL: f32 = 0.6;
const SHOT_DAMAGE: f32 = 34.0;
const MAX_HEALTH: f32 = 100.0;
const RESPAWN_DELAY: f32 = 8.0;
/// Radius of the area soldiers spawn in around a capture point.
const SPAWN_SPREAD: f32 = 20.0;
/// Each base's spawn area, as fractions of the map: this deep from its edge, this wide across the middle.
const BASE_DEPTH: f32 = 0.10;
const BASE_WIDTH: f32 = 0.5;
/// Soldiers closer than this push apart, up to this fast.
const PERSONAL_SPACE: f32 = 1.2;
const SEPARATION_SPEED: f32 = 4.0;
/// Teammates a soldier reacts to at once. Bounds the cost in a packed crowd, where everyone has dozens of
/// neighbours within reach; a handful is enough to spread out.
const SEPARATION_NEIGHBOURS: u32 = 6;
const CAPTURE_RADIUS: f32 = 25.0;
const CAPTURE_SECONDS: f32 = 10.0;
/// Touch attackers run at the nearest enemy, slower than a running soldier so you can get away, and strike
/// once they're within reach.
const TOUCH_RANGE: f32 = 1.5;
const TOUCH_INTERVAL: f32 = 1.0;
const TOUCH_DAMAGE: f32 = 20.0;
const TOUCH_SPEED: f32 = 4.0;
/// A human player's rifle: faster and more accurate than a bot's. It hits the first enemy along the aim
/// whose body is within this many metres of the line, plus this much per metre of distance, which
/// forgives the client drawing enemies a few ticks in the past.
pub const PLAYER_FIRE_INTERVAL: f32 = 0.12;
const PLAYER_HIT_CHANCE: f32 = 0.9;
const PLAYER_AIM_WIDTH: f32 = 0.75;
const PLAYER_AIM_SPREAD: f32 = 0.05;

// Salts that keep each random decision independent.
const SALT_SHOT: u64 = 1;
const SALT_STRAFE: u64 = 2;
const SALT_SPAWN: u64 = 3;
const SALT_SPOT: u64 = 4;
const SALT_SPLIT: u64 = 5;

/// How a team's bots fight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attack {
    /// Rifles: shoot the nearest enemy in range while strafing.
    Shoot,
    /// Run at the nearest enemy in sight and strike on contact.
    Touch,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Side length of the square map, in metres.
    pub map_size: f32,
    /// Bots per team. Team 0 is your side and starts holding every capture point; team 1 is the horde.
    pub bots: [usize; TEAMS],
    /// How each team's bots fight. Players always shoot.
    pub attacks: [Attack; TEAMS],
    pub capture_points: usize,
    pub tick_rate: u32,
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            map_size: 1024.0,
            bots: [14, 986],
            attacks: [Attack::Shoot, Attack::Touch],
            capture_points: 5,
            tick_rate: 30,
            seed: 0x5EED,
        }
    }
}

/// An attack made in the parallel pass, applied in chunk order during resolve. Ranged ones also go to
/// clients as shots.
#[derive(Clone, Copy)]
struct Strike {
    shot: Shot,
    damage: f32,
    ranged: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CapturePoint {
    pub x: f32,
    pub z: f32,
    /// 0 or 1 for the team holding it, or [`NEUTRAL`].
    pub owner: u8,
    /// -1 when team 0 fully holds the point, +1 when team 1 does.
    pub control: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub kills: [u64; TEAMS],
    pub alive: [u32; TEAMS],
    pub points_held: [u32; TEAMS],
}

pub struct World {
    cfg: Config,
    dt: f32,
    tick: u64,
    team: Vec<u8>,
    pos_x: Vec<f32>,
    pos_z: Vec<f32>,
    /// Facing in radians: 0 faces +z, positive turns toward +x.
    yaw: Vec<f32>,
    health: Vec<f32>,
    cooldown: Vec<f32>,
    respawn: Vec<f32>,
    target: Vec<u32>,
    /// Soldiers controlled by a connected player instead of the AI, and their latest controls.
    human: Vec<bool>,
    input: Vec<PlayerInput>,
    /// Living soldiers, one group per team: coarse cells for sighting, fine cells for crowd separation.
    grid: Grid,
    crowd: Grid,
    points: Vec<CapturePoint>,
    /// Per team, the capture points its soldiers head for this tick.
    objectives: [Vec<usize>; TEAMS],
    /// Attacks made, one buffer per parallel chunk; damage is applied from them in chunk order.
    fired: Vec<Vec<Strike>>,
    /// Every shot of the last step (not touch attacks), for clients.
    shots: Vec<Shot>,
    stats: Stats,
}

impl World {
    pub fn new(cfg: Config) -> World {
        let n = cfg.bots[0] + cfg.bots[1];
        let size = cfg.map_size;
        let count = cfg.capture_points.max(1);
        let points = (0..count)
            .map(|k| {
                let t = (k as f32 + 0.5) / count as f32;
                CapturePoint {
                    x: size * (0.25 + 0.5 * t),
                    z: size * (0.5 + 0.3 * (t * std::f32::consts::TAU).sin()),
                    owner: 0,
                    control: -1.0,
                }
            })
            .collect();
        let mut world = World {
            dt: 1.0 / cfg.tick_rate as f32,
            tick: 0,
            team: (0..n).map(|i| u8::from(i >= cfg.bots[0])).collect(),
            pos_x: vec![0.0; n],
            pos_z: vec![0.0; n],
            yaw: vec![0.0; n],
            health: vec![MAX_HEALTH; n],
            cooldown: vec![0.0; n],
            respawn: vec![0.0; n],
            target: vec![NO_TARGET; n],
            human: vec![false; n],
            input: vec![PlayerInput::default(); n],
            grid: Grid::new(size, GRID_CELL, TEAMS),
            // Keep the fine grid to at most 256 cells a side on very large maps.
            crowd: Grid::new(size, CROWD_CELL.max(size / 256.0), TEAMS),
            points,
            objectives: [Vec::new(), Vec::new()],
            fired: (0..n.div_ceil(CHUNK)).map(|_| Vec::new()).collect(),
            shots: Vec::new(),
            stats: Stats::default(),
            cfg,
        };
        for i in 0..n {
            world.place_at_spawn(i);
        }
        world.update_stats();
        world
    }

    pub fn soldiers(&self) -> usize {
        self.team.len()
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn points(&self) -> &[CapturePoint] {
        &self.points
    }

    /// Each team attacks the points it doesn't hold; a team holding everything defends all of them.
    fn update_objectives(&mut self) {
        for (team, list) in self.objectives.iter_mut().enumerate() {
            list.clear();
            list.extend((0..self.points.len()).filter(|&p| self.points[p].owner != team as u8));
            if list.is_empty() {
                list.extend(0..self.points.len());
            }
        }
    }

    /// The parallel pass: sensing, targeting, movement and firing for every living soldier.
    fn think(&mut self, pool: &JobPool) {
        let (dt, tick, seed, map, attacks) = (self.dt, self.tick, self.cfg.seed, self.cfg.map_size, self.cfg.attacks);
        let World {
            team, pos_x, pos_z, yaw, health, cooldown, target, human, input, grid, crowd, points, objectives, fired, ..
        } = self;
        let (team, health, human, input) = (&*team, &*health, &*human, &*input);
        let (grid, crowd, points, objectives) = (&*grid, &*crowd, &*points, &*objectives);
        let n = team.len();
        let pos_x = DisjointSlice::new(pos_x);
        let pos_z = DisjointSlice::new(pos_z);
        let yaw = DisjointSlice::new(yaw);
        let cooldown = DisjointSlice::new(cooldown);
        let target = DisjointSlice::new(target);
        let fired = DisjointSlice::new(fired);

        pool.for_ranges(n, CHUNK, |range| {
            // SAFETY: this task owns every per-soldier index in `range` and the shot buffer of its chunk.
            let out = unsafe { fired.get_mut(range.start / CHUNK) };
            for i in range {
                // SAFETY: as above.
                let (x, z, facing, cool, tgt) = unsafe {
                    (pos_x.get_mut(i), pos_z.get_mut(i), yaw.get_mut(i), cooldown.get_mut(i), target.get_mut(i))
                };
                if health[i] <= 0.0 {
                    *tgt = NO_TARGET;
                    continue;
                }
                *cool = (*cool - dt).max(0.0);
                let my_team = team[i];
                let enemy_team = (my_team + 1) % TEAMS as u8;

                if human[i] {
                    let controls = &input[i];
                    let (mut mx, mut mz) = (controls.move_x, controls.move_z);
                    let len = (mx * mx + mz * mz).sqrt();
                    if len > 1.0 {
                        (mx, mz) = (mx / len, mz / len);
                    }
                    *facing = controls.yaw;
                    *tgt = NO_TARGET;
                    if controls.fire && *cool <= 0.0 {
                        *cool = PLAYER_FIRE_INTERVAL;
                        // At whatever is in the crosshair, or at nothing in particular.
                        let aimed = first_in_line(grid, *x, *z, controls.yaw, enemy_team);
                        let hit = aimed.is_some() && rng::unit(seed, tick, i, SALT_SHOT) < PLAYER_HIT_CHANCE;
                        *tgt = aimed.unwrap_or(NO_TARGET);
                        let shot = Shot { shooter: i as u32, target: *tgt, hit, kill: false };
                        out.push(Strike { shot, damage: SHOT_DAMAGE, ranged: true });
                    }
                    *x = (*x + mx * RUN_SPEED * dt).clamp(0.0, map);
                    *z = (*z + mz * RUN_SPEED * dt).clamp(0.0, map);
                    continue;
                }

                let attack = attacks[my_team as usize];
                let speed = if attack == Attack::Touch { TOUCH_SPEED } else { RUN_SPEED };
                let (vx, vz) = match (attack, grid.nearest_in_group(*x, *z, enemy_team, SIGHT)) {
                    (Attack::Shoot, Some((slot, d2))) if d2 <= FIRE_RANGE * FIRE_RANGE => {
                        let enemy = grid.entity(slot);
                        *tgt = enemy;
                        let d = d2.sqrt().max(0.01);
                        let (dx, dz) = ((grid.x(slot) - *x) / d, (grid.z(slot) - *z) / d);
                        *facing = dx.atan2(dz);
                        if *cool <= 0.0 {
                            *cool = FIRE_INTERVAL;
                            let chance = 0.05 + 0.4 * (1.0 - d / FIRE_RANGE);
                            let hit = rng::unit(seed, tick, i, SALT_SHOT) < chance;
                            let shot = Shot { shooter: i as u32, target: enemy, hit, kill: false };
                            out.push(Strike { shot, damage: SHOT_DAMAGE, ranged: true });
                        }
                        // Strafe while fighting, switching direction every couple of seconds.
                        let side = if rng::unit(seed, tick / 60, i, SALT_STRAFE) < 0.5 { 1.0 } else { -1.0 };
                        (-dz * side * COMBAT_SPEED, dx * side * COMBAT_SPEED)
                    }
                    (Attack::Touch, Some((slot, d2))) => {
                        // Charge, and strike once within reach.
                        let enemy = grid.entity(slot);
                        *tgt = enemy;
                        let d = d2.sqrt().max(0.01);
                        let (dx, dz) = ((grid.x(slot) - *x) / d, (grid.z(slot) - *z) / d);
                        *facing = dx.atan2(dz);
                        if d > TOUCH_RANGE {
                            (dx * speed, dz * speed)
                        } else {
                            if *cool <= 0.0 {
                                *cool = TOUCH_INTERVAL;
                                let shot = Shot { shooter: i as u32, target: enemy, hit: true, kill: false };
                                out.push(Strike { shot, damage: TOUCH_DAMAGE, ranged: false });
                            }
                            (0.0, 0.0)
                        }
                    }
                    (_, Some((slot, d2))) => {
                        *tgt = NO_TARGET;
                        let d = d2.sqrt().max(0.01);
                        ((grid.x(slot) - *x) / d * speed, (grid.z(slot) - *z) / d * speed)
                    }
                    (_, None) => {
                        *tgt = NO_TARGET;
                        head_for_objective(*x, *z, &objectives[my_team as usize], points, i, seed, speed)
                    }
                };
                if *tgt == NO_TARGET && (vx != 0.0 || vz != 0.0) {
                    *facing = vx.atan2(vz);
                }
                // Keep personal space from teammates, without exceeding a little over running speed.
                let (sx, sz) = separation(crowd, *x, *z, my_team, i, seed);
                let (mut vx, mut vz) = (vx + sx * SEPARATION_SPEED, vz + sz * SEPARATION_SPEED);
                let speed2 = vx * vx + vz * vz;
                let max_speed = speed * 1.2;
                if speed2 > max_speed * max_speed {
                    let k = max_speed / speed2.sqrt();
                    (vx, vz) = (vx * k, vz * k);
                }
                *x = (*x + vx * dt).clamp(0.0, map);
                *z = (*z + vz * dt).clamp(0.0, map);
            }
        });
    }

    /// Serial: damage in chunk order, then deaths and respawns, capture points and stats.
    fn resolve(&mut self) {
        self.shots.clear();
        for buffer in &mut self.fired {
            for strike in buffer.drain(..) {
                let mut shot = strike.shot;
                if shot.hit {
                    let victim = shot.target as usize;
                    if self.health[victim] > 0.0 {
                        self.health[victim] -= strike.damage;
                        if self.health[victim] <= 0.0 {
                            shot.kill = true;
                            self.stats.kills[self.team[shot.shooter as usize] as usize] += 1;
                            self.respawn[victim] = RESPAWN_DELAY;
                        }
                    } else {
                        // Already killed by an earlier attack this tick: this one passes through.
                        shot.hit = false;
                    }
                }
                if strike.ranged {
                    self.shots.push(shot);
                }
            }
        }

        for i in 0..self.team.len() {
            if self.health[i] <= 0.0 {
                self.respawn[i] -= self.dt;
                if self.respawn[i] <= 0.0 {
                    self.place_at_spawn(i);
                }
            }
        }

        let rate = self.dt / CAPTURE_SECONDS;
        let mut present = [0u32; TEAMS];
        for p in &mut self.points {
            self.grid.count_near(p.x, p.z, CAPTURE_RADIUS, &mut present);
            match (present[0] > 0, present[1] > 0) {
                (true, false) => p.control = (p.control - rate).max(-1.0),
                (false, true) => p.control = (p.control + rate).min(1.0),
                _ => {}
            }
            p.owner = if p.control <= -1.0 {
                0
            } else if p.control >= 1.0 {
                1
            } else if (p.owner == 0 && p.control > 0.0) || (p.owner == 1 && p.control < 0.0) {
                NEUTRAL
            } else {
                p.owner
            };
        }

        self.update_stats();
    }

    fn update_stats(&mut self) {
        self.stats.alive = [0; TEAMS];
        for (&team, &health) in self.team.iter().zip(&self.health) {
            if health > 0.0 {
                self.stats.alive[team as usize] += 1;
            }
        }
        self.stats.points_held = [0; TEAMS];
        for p in &self.points {
            if (p.owner as usize) < TEAMS {
                self.stats.points_held[p.owner as usize] += 1;
            }
        }
    }

    /// Respawns soldier `i` at full health near a capture point its team holds, or at its base.
    fn place_at_spawn(&mut self, i: usize) {
        let team = self.team[i];
        let size = self.cfg.map_size;
        let held = self.points.iter().filter(|p| p.owner == team).count();
        let pick = rng::hash(self.cfg.seed ^ rng::hash(self.tick ^ ((i as u64) << 24) ^ SALT_SPAWN));
        let (u, v) = (
            rng::unit(self.cfg.seed, self.tick, i, SALT_SPAWN),
            rng::unit(self.cfg.seed, self.tick, i, SALT_SPAWN + 100),
        );
        let (x, z) = if held == 0 {
            // Anywhere in the team's base area along its edge of the map.
            let depth = u * BASE_DEPTH;
            let x = if team == 0 { 0.02 + depth } else { 0.98 - depth };
            (size * x, size * (0.5 - BASE_WIDTH / 2.0 + v * BASE_WIDTH))
        } else {
            // Spread evenly over a disc around one of the team's capture points.
            let nth = (pick % held as u64) as usize;
            let p = self.points.iter().filter(|p| p.owner == team).nth(nth).expect("counted above");
            let (angle, radius) = (u * std::f32::consts::TAU, v.sqrt() * SPAWN_SPREAD);
            (p.x + angle.cos() * radius, p.z + angle.sin() * radius)
        };
        self.pos_x[i] = x.clamp(0.0, size);
        self.pos_z[i] = z.clamp(0.0, size);
        self.health[i] = MAX_HEALTH;
        self.cooldown[i] = 0.0;
        self.target[i] = NO_TARGET;
    }
}

impl Simulation for World {
    fn tick_rate(&self) -> u32 {
        self.cfg.tick_rate
    }

    fn step(&mut self, pool: &JobPool) -> Phases {
        let mut timer = PhaseTimer::start();
        let World { team, pos_x, pos_z, health, grid, crowd, .. } = self;
        grid.rebuild(pos_x, pos_z, |i| (health[i] > 0.0).then_some(team[i]));
        crowd.rebuild(pos_x, pos_z, |i| (health[i] > 0.0).then_some(team[i]));
        self.update_objectives();
        timer.mark("grid");
        self.think(pool);
        timer.mark("think");
        self.resolve();
        self.tick += 1;
        timer.mark("resolve");
        timer.finish()
    }

    fn status(&self) -> String {
        let s = self.stats;
        format!(
            "alive {}/{}  kills {}/{}  points {}/{}",
            s.alive[0], s.alive[1], s.kills[0], s.kills[1], s.points_held[0], s.points_held[1]
        )
    }

    /// A hash of every soldier's position and health.
    fn checksum(&self) -> u64 {
        let mut h = self.tick;
        for i in 0..self.team.len() {
            h = rng::hash(h ^ u64::from(self.pos_x[i].to_bits()) ^ (u64::from(self.pos_z[i].to_bits()) << 32));
            h = rng::hash(h ^ u64::from(self.health[i].to_bits()));
        }
        h
    }
}

/// A joining player takes over one of your side's bots, the way BF2's coop mode swapped a player in for a
/// bot; when they leave, the AI takes it back.
impl Replicated for World {
    fn world_size(&self) -> f32 {
        self.cfg.map_size
    }

    fn entity_count(&self) -> usize {
        self.team.len()
    }

    fn entity_state(&self, entity: usize) -> EntityState {
        EntityState {
            x: self.pos_x[entity],
            z: self.pos_z[entity],
            yaw: self.yaw[entity],
            team: self.team[entity],
            kind: 0,
            alive: self.health[entity] > 0.0,
            health: (self.health[entity] / MAX_HEALTH).max(0.0),
        }
    }

    fn spawn_player(&mut self) -> Option<u32> {
        let free = |w: &World, i: usize| w.team[i] == 0 && !w.human[i];
        let pick = (0..self.team.len())
            .find(|&i| free(self, i) && self.health[i] > 0.0)
            .or_else(|| (0..self.team.len()).find(|&i| free(self, i)))?;
        self.human[pick] = true;
        self.input[pick] = PlayerInput::default();
        Some(pick as u32)
    }

    fn despawn_player(&mut self, entity: u32) {
        if let Some(h) = self.human.get_mut(entity as usize) {
            *h = false;
        }
    }

    fn apply_input(&mut self, entity: u32, input: &PlayerInput) {
        if let Some(slot) = self.input.get_mut(entity as usize) {
            *slot = *input;
        }
    }

    fn shots(&self) -> &[Shot] {
        &self.shots
    }
}

/// Velocity toward soldier `i`'s own spot on the nearest of `objectives`; zero once there. Each soldier's
/// spot is fixed for the game, spread over the capture area, so a crowd covers a point instead of piling
/// onto its centre.
fn head_for_objective(
    x: f32,
    z: f32,
    objectives: &[usize],
    points: &[CapturePoint],
    i: usize,
    seed: u64,
    speed: f32,
) -> (f32, f32) {
    let nearest = objectives.iter().map(|&p| &points[p]).min_by(|a, b| {
        let da = (a.x - x).powi(2) + (a.z - z).powi(2);
        let db = (b.x - x).powi(2) + (b.z - z).powi(2);
        da.total_cmp(&db)
    });
    let Some(p) = nearest else { return (0.0, 0.0) };
    let angle = rng::unit(seed, 0, i, SALT_SPOT) * std::f32::consts::TAU;
    let radius = rng::unit(seed, 0, i, SALT_SPOT + 100).sqrt() * CAPTURE_RADIUS * 0.85;
    let (dx, dz) = (p.x + angle.sin() * radius - x, p.z + angle.cos() * radius - z);
    let d = (dx * dx + dz * dz).sqrt();
    if d < 1.0 { (0.0, 0.0) } else { (dx / d * speed, dz / d * speed) }
}

/// What a player at (x, z) aiming along `yaw` would hit: the nearest soldier of `team` ahead of them within
/// rifle range whose body is close enough to the line of fire. Visiting order is fixed, so ties are
/// deterministic.
fn first_in_line(grid: &Grid, x: f32, z: f32, yaw: f32, team: u8) -> Option<u32> {
    let (ax, az) = yaw.sin_cos();
    let mut best: Option<(f32, u32)> = None;
    grid.for_each_near(x, z, team, FIRE_RANGE, |slot, dx, dz, _| {
        // (dx, dz) points from the enemy back to the shooter.
        let along = -(dx * ax + dz * az);
        let off = (dx * az - dz * ax).abs();
        if along > 0.0 && off <= PLAYER_AIM_WIDTH + along * PLAYER_AIM_SPREAD && best.is_none_or(|(b, _)| along < b) {
            best = Some((along, grid.entity(slot)));
        }
        true
    });
    best.map(|(_, e)| e)
}

/// Push away from teammates within personal space: the closer, the harder. Soldiers on exactly the same
/// spot split along a direction fixed for that pair, so the result doesn't depend on thread order.
fn separation(crowd: &Grid, x: f32, z: f32, team: u8, me: usize, seed: u64) -> (f32, f32) {
    let (mut sx, mut sz, mut seen) = (0.0f32, 0.0f32, 0u32);
    crowd.for_each_near(x, z, team, PERSONAL_SPACE, |slot, dx, dz, d2| {
        let other = crowd.entity(slot) as usize;
        if other == me {
            return true;
        }
        if d2 < 1e-8 {
            let (lo, hi) = (me.min(other), me.max(other));
            let angle = rng::unit(seed, lo as u64, hi, SALT_SPLIT) * std::f32::consts::TAU;
            let away = if me == lo { 1.0 } else { -1.0 };
            sx += angle.sin() * away;
            sz += angle.cos() * away;
        } else {
            let d = d2.sqrt();
            let push = 1.0 - d / PERSONAL_SPACE;
            sx += dx / d * push;
            sz += dz / d * push;
        }
        seen += 1;
        seen < SEPARATION_NEIGHBOURS
    });
    (sx, sz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(workers: usize, ticks: u32, bots: [usize; TEAMS]) -> (u64, Stats) {
        let pool = JobPool::unpinned(workers);
        let mut world = World::new(Config { bots, ..Config::default() });
        for _ in 0..ticks {
            world.step(&pool);
        }
        (world.checksum(), world.stats())
    }

    #[test]
    fn result_does_not_depend_on_thread_count() {
        // Long enough for the horde to reach the defenders, so combat and respawns are covered.
        let single = run(0, 2400, [100, 900]);
        assert_eq!(single, run(3, 2400, [100, 900]));
        assert_eq!(single, run(7, 2400, [100, 900]));
    }

    #[test]
    fn a_player_takes_over_a_defender_and_follows_input() {
        let pool = JobPool::unpinned(2);
        let mut world = World::new(Config { bots: [3, 50], ..Config::default() });
        let me = world.spawn_player().expect("a defender is free") as usize;
        assert_eq!(world.team[me], 0);
        let start = (world.pos_x[me], world.pos_z[me]);
        world.apply_input(me as u32, &PlayerInput { move_x: 1.0, move_z: 0.0, yaw: 1.0, fire: false });
        for _ in 0..30 {
            world.step(&pool);
        }
        let state = world.entity_state(me);
        assert!((state.x - start.0 - RUN_SPEED).abs() < 0.01, "moved {} m in 1 s", state.x - start.0);
        assert!((state.z - start.1).abs() < 1e-3);
        assert_eq!(state.yaw, 1.0);

        world.despawn_player(me as u32);
        assert!(!world.human[me]);
    }

    #[test]
    fn rifle_kills_are_reported_as_shots_and_touches_are_not() {
        let pool = JobPool::unpinned(3);
        let mut world = World::new(Config { bots: [500, 500], ..Config::default() });
        let (mut rifle_kills, mut misses) = (0u64, 0u64);
        for _ in 0..3000 {
            world.step(&pool);
            for shot in world.shots() {
                assert_eq!(world.team[shot.shooter as usize], 0, "the horde attacks by touch: {shot:?}");
                assert!(shot.hit || !shot.kill, "a miss can't kill: {shot:?}");
                rifle_kills += u64::from(shot.kill);
                misses += u64::from(!shot.hit);
            }
        }
        assert_eq!(rifle_kills, world.stats().kills[0]);
        assert!(misses > 0);
    }

    #[test]
    fn the_horde_strikes_only_within_reach() {
        let pool = JobPool::unpinned(1);
        let mut world = World::new(Config { bots: [1, 2], ..Config::default() });
        // A player standing still without firing, one horde soldier at arm's length and another 40 m away.
        let me = world.spawn_player().expect("a defender is free") as usize;
        for (i, x) in [(me, 500.0), (1, 501.0), (2, 540.0)] {
            (world.pos_x[i], world.pos_z[i]) = (x, 500.0);
        }
        world.step(&pool);
        assert_eq!(world.health[me], MAX_HEALTH - TOUCH_DAMAGE, "one strike, from the close one");
        let far = world.pos_x[2];
        assert!((540.0 - far - TOUCH_SPEED / 30.0).abs() < 1e-3, "the far one charges: now at {far}");
        assert!(world.shots().is_empty(), "touch attacks aren't shots");
        // The next strike waits about a second.
        for _ in 0..25 {
            world.step(&pool);
        }
        assert_eq!(world.health[me], MAX_HEALTH - TOUCH_DAMAGE);
        for _ in 0..10 {
            world.step(&pool);
        }
        assert_eq!(world.health[me], MAX_HEALTH - 2.0 * TOUCH_DAMAGE);
    }

    #[test]
    fn a_player_hits_what_is_in_the_crosshair_not_the_nearest_enemy() {
        let pool = JobPool::unpinned(1);
        let mut world = World::new(Config { bots: [1, 2], ..Config::default() });
        let me = world.spawn_player().expect("a defender is free");
        // Facing +z: the nearest enemy is 10 m away, ahead and to the right; a farther one is 30 m straight
        // ahead.
        for (i, x, z) in [(me as usize, 500.0, 500.0), (1, 506.0, 508.0), (2, 500.0, 530.0)] {
            (world.pos_x[i], world.pos_z[i]) = (x, z);
        }
        world.apply_input(me, &PlayerInput { move_x: 0.0, move_z: 0.0, yaw: 0.0, fire: true });
        world.step(&pool);
        let shot = world.shots().iter().find(|s| s.shooter == me).copied().expect("the player fired");
        assert_eq!(shot.target, 2);
    }

    #[test]
    fn both_sides_score_kills_against_the_horde() {
        // The horde needs about a minute to reach the first capture point. (In an even fight it never gets
        // close: rifles cut down soldiers that have to run into touching range.)
        let (_, stats) = run(3, 3000, Config::default().bots);
        assert!(stats.kills[0] > 0 && stats.kills[1] > 0, "{stats:?}");
    }
}
