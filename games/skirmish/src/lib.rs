//! Skirmish: the engine's first test game. A small defending side holds capture points against a horde.
//!
//! Soldiers are stored as structure-of-arrays. A step is a serial grid rebuild, one parallel pass over all
//! soldiers in fixed 64-soldier chunks, and a serial resolve. A task in the parallel pass writes only its
//! own soldiers and its own chunk's hit buffer; damage to other soldiers is applied afterwards in chunk
//! order, so the result never depends on how many threads ran it (docs/DESIGN.md, "Determinism contract").

use horde_core::{PhaseTimer, Phases, Simulation, rng};
use horde_jobs::{DisjointSlice, JobPool};
use horde_spatial::Grid;

pub const TEAMS: usize = 2;
/// `CapturePoint::owner` when neither team holds the point.
pub const NEUTRAL: u8 = 2;
const NO_TARGET: u32 = u32::MAX;
/// Soldiers per parallel chunk: large enough to amortise claiming a chunk, small enough to balance
/// 17 threads at 1,000 soldiers.
const CHUNK: usize = 64;
const GRID_CELL: f32 = 32.0;

// Combat tuning: metres, metres per second, seconds.
const SIGHT: f32 = 90.0;
const FIRE_RANGE: f32 = 60.0;
const RUN_SPEED: f32 = 5.0;
const COMBAT_SPEED: f32 = 1.5;
const FIRE_INTERVAL: f32 = 0.6;
const SHOT_DAMAGE: f32 = 34.0;
const MAX_HEALTH: f32 = 100.0;
const RESPAWN_DELAY: f32 = 8.0;
const SPAWN_SPREAD: f32 = 15.0;
const CAPTURE_RADIUS: f32 = 25.0;
const CAPTURE_SECONDS: f32 = 10.0;

// Salts that keep each random decision independent.
const SALT_SHOT: u64 = 1;
const SALT_STRAFE: u64 = 2;
const SALT_SPAWN: u64 = 3;

#[derive(Clone, Debug)]
pub struct Config {
    /// Side length of the square map, in metres.
    pub map_size: f32,
    /// Bots per team. Team 0 is your side and starts holding every capture point; team 1 is the horde.
    pub bots: [usize; TEAMS],
    pub capture_points: usize,
    pub tick_rate: u32,
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config { map_size: 1024.0, bots: [14, 986], capture_points: 5, tick_rate: 30, seed: 0x5EED }
    }
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

#[derive(Clone, Copy)]
struct Hit {
    attacker: u32,
    target: u32,
    damage: f32,
}

pub struct World {
    cfg: Config,
    dt: f32,
    tick: u64,
    team: Vec<u8>,
    pos_x: Vec<f32>,
    pos_z: Vec<f32>,
    health: Vec<f32>,
    cooldown: Vec<f32>,
    respawn: Vec<f32>,
    target: Vec<u32>,
    /// Living soldiers, one group per team.
    grid: Grid,
    points: Vec<CapturePoint>,
    /// Per team, the capture points its soldiers head for this tick.
    objectives: [Vec<usize>; TEAMS],
    /// One buffer per parallel chunk.
    hits: Vec<Vec<Hit>>,
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
            health: vec![MAX_HEALTH; n],
            cooldown: vec![0.0; n],
            respawn: vec![0.0; n],
            target: vec![NO_TARGET; n],
            grid: Grid::new(size, GRID_CELL, TEAMS),
            points,
            objectives: [Vec::new(), Vec::new()],
            hits: (0..n.div_ceil(CHUNK)).map(|_| Vec::new()).collect(),
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
        let (dt, tick, seed, map) = (self.dt, self.tick, self.cfg.seed, self.cfg.map_size);
        let World { team, pos_x, pos_z, health, cooldown, target, grid, points, objectives, hits, .. } = self;
        let (team, health, grid, points, objectives) = (&*team, &*health, &*grid, &*points, &*objectives);
        let n = team.len();
        let pos_x = DisjointSlice::new(pos_x);
        let pos_z = DisjointSlice::new(pos_z);
        let cooldown = DisjointSlice::new(cooldown);
        let target = DisjointSlice::new(target);
        let hits = DisjointSlice::new(hits);

        pool.for_ranges(n, CHUNK, |range| {
            // SAFETY: this task owns every per-soldier index in `range` and the hit buffer of its chunk.
            let out = unsafe { hits.get_mut(range.start / CHUNK) };
            for i in range {
                // SAFETY: as above.
                let (x, z, cool, tgt) =
                    unsafe { (pos_x.get_mut(i), pos_z.get_mut(i), cooldown.get_mut(i), target.get_mut(i)) };
                if health[i] <= 0.0 {
                    *tgt = NO_TARGET;
                    continue;
                }
                *cool = (*cool - dt).max(0.0);
                let my_team = team[i];
                let enemy_team = (my_team + 1) % TEAMS as u8;
                let (vx, vz) = match grid.nearest_in_group(*x, *z, enemy_team, SIGHT) {
                    Some((slot, d2)) if d2 <= FIRE_RANGE * FIRE_RANGE => {
                        let enemy = grid.entity(slot);
                        *tgt = enemy;
                        let d = d2.sqrt().max(0.01);
                        let (dx, dz) = ((grid.x(slot) - *x) / d, (grid.z(slot) - *z) / d);
                        if *cool <= 0.0 {
                            *cool = FIRE_INTERVAL;
                            let chance = 0.05 + 0.4 * (1.0 - d / FIRE_RANGE);
                            if rng::unit(seed, tick, i, SALT_SHOT) < chance {
                                out.push(Hit { attacker: i as u32, target: enemy, damage: SHOT_DAMAGE });
                            }
                        }
                        // Strafe while fighting, switching direction every couple of seconds.
                        let side = if rng::unit(seed, tick / 60, i, SALT_STRAFE) < 0.5 { 1.0 } else { -1.0 };
                        (-dz * side * COMBAT_SPEED, dx * side * COMBAT_SPEED)
                    }
                    Some((slot, d2)) => {
                        *tgt = NO_TARGET;
                        let d = d2.sqrt().max(0.01);
                        ((grid.x(slot) - *x) / d * RUN_SPEED, (grid.z(slot) - *z) / d * RUN_SPEED)
                    }
                    None => {
                        *tgt = NO_TARGET;
                        head_for_objective(*x, *z, &objectives[my_team as usize], points)
                    }
                };
                *x = (*x + vx * dt).clamp(0.0, map);
                *z = (*z + vz * dt).clamp(0.0, map);
            }
        });
    }

    /// Serial: damage in chunk order, then deaths and respawns, capture points and stats.
    fn resolve(&mut self) {
        for buffer in &mut self.hits {
            for hit in buffer.drain(..) {
                let victim = hit.target as usize;
                if self.health[victim] > 0.0 {
                    self.health[victim] -= hit.damage;
                    if self.health[victim] <= 0.0 {
                        self.stats.kills[self.team[hit.attacker as usize] as usize] += 1;
                        self.respawn[victim] = RESPAWN_DELAY;
                    }
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
        let (cx, cz) = if held == 0 {
            (if team == 0 { size * 0.08 } else { size * 0.92 }, size * 0.5)
        } else {
            let nth = (pick % held as u64) as usize;
            let p = self.points.iter().filter(|p| p.owner == team).nth(nth).expect("counted above");
            (p.x, p.z)
        };
        let angle = rng::unit(self.cfg.seed, self.tick, i, SALT_SPAWN) * std::f32::consts::TAU;
        let radius = rng::unit(self.cfg.seed, self.tick, i, SALT_SPAWN + 100) * SPAWN_SPREAD;
        self.pos_x[i] = (cx + angle.cos() * radius).clamp(0.0, size);
        self.pos_z[i] = (cz + angle.sin() * radius).clamp(0.0, size);
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
        let World { team, pos_x, pos_z, health, grid, .. } = self;
        grid.rebuild(pos_x, pos_z, |i| (health[i] > 0.0).then_some(team[i]));
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

/// Velocity toward the nearest of `objectives`; zero once standing on it.
fn head_for_objective(x: f32, z: f32, objectives: &[usize], points: &[CapturePoint]) -> (f32, f32) {
    let nearest = objectives.iter().map(|&p| &points[p]).min_by(|a, b| {
        let da = (a.x - x).powi(2) + (a.z - z).powi(2);
        let db = (b.x - x).powi(2) + (b.z - z).powi(2);
        da.total_cmp(&db)
    });
    let Some(p) = nearest else { return (0.0, 0.0) };
    let (dx, dz) = (p.x - x, p.z - z);
    let d = (dx * dx + dz * dz).sqrt();
    if d < CAPTURE_RADIUS * 0.5 { (0.0, 0.0) } else { (dx / d * RUN_SPEED, dz / d * RUN_SPEED) }
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
    fn both_sides_score_kills_in_an_even_fight() {
        // The attackers need about 45 simulated seconds to reach the first capture point.
        let (_, stats) = run(3, 3000, [500, 500]);
        assert!(stats.kills[0] > 0 && stats.kills[1] > 0, "{stats:?}");
    }
}
