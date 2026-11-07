//! Short-lived feedback over the battle: tracers for shots, a crosshair, hit and kill markers, and a red
//! flash when you're hurt.
//!
//! Other soldiers' shots arrive from the server and wait until the render clock reaches the tick they were
//! fired on. Each tracer then flies between the soldiers as they're drawn at that moment, so a kill lands
//! just as its target drops. Your own tracers leave the instant you fire, at the weapon's rate, so firing
//! feels immediate; the server's verdict on your shots drives the hit markers.

use crate::render::{OverlayVertex, Tracer};
use crate::view::EntityView;
use glam::{Vec2, Vec3};
use horde_core::rng;
use horde_net::{ReceivedShot, Shot};
use std::collections::VecDeque;

/// Tracers fly slower than real bullets, so the eye can follow them, as streaks this long.
const TRACER_SPEED: f32 = 350.0;
const TRACER_STREAK: f32 = 9.0;
/// Shots leave and arrive at chest height, starting at the muzzle rather than inside the shooter.
const CHEST_HEIGHT: f32 = 1.3;
const MUZZLE: f32 = 0.5;
/// A miss passes this far beside its target, on a side fixed for that shot, and flies on this far.
const MISS_SIDE: (f32, f32) = (0.5, 1.8);
const MISS_PAST: f32 = 25.0;
/// Caps on tracers in flight and shots waiting for the clock; past them new ones are dropped.
const MAX_FLIGHTS: usize = 4096;
const MAX_PENDING: usize = 8192;
/// Shots this many ticks behind the render clock are dropped rather than drawn late.
const STALE_TICKS: f64 = 15.0;
const HIT_SECONDS: f32 = 0.2;
const KILL_SECONDS: f32 = 0.5;
const HURT_SECONDS: f32 = 0.4;
const SALT_MISS: u64 = 1;

/// Tracer colours: yours, then by the shooter's team.
const OWN_TRACER: [f32; 3] = [1.0, 0.95, 0.6];
const TEAM_TRACERS: [[f32; 3]; 4] = [[1.0, 0.8, 0.3], [1.0, 0.35, 0.15], [0.5, 1.0, 0.4], [0.9, 0.9, 0.9]];
const HIT_COLOR: [f32; 3] = [1.0, 1.0, 1.0];
const KILL_COLOR: [f32; 3] = [1.0, 0.25, 0.2];
const HURT_COLOR: [f32; 3] = [0.8, 0.05, 0.05];

/// The player's weapon, as far as the client needs to know it. Games fill it in from their own rules.
#[derive(Clone, Copy, Debug)]
pub struct Weapon {
    /// Seconds between shots. The server fires on whole ticks, so the client rounds this up to them.
    pub fire_interval: f32,
    /// How far a shot at nothing in particular flies.
    pub range: f32,
}

struct Flight {
    from: Vec3,
    dir: Vec3,
    length: f32,
    age: f32,
    color: [f32; 3],
}

#[derive(Default)]
pub struct Effects {
    pending: VecDeque<ReceivedShot>,
    flights: Vec<Flight>,
    /// Seconds until your weapon fires again.
    cooldown: f32,
    /// Seconds left on each marker.
    hit: f32,
    kill: f32,
    hurt: f32,
    /// Your soldier's health last frame, if it's alive and known.
    health: Option<f32>,
}

impl Effects {
    /// Forgets everything, for a new game.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.flights.clear();
        (self.cooldown, self.hit, self.kill, self.hurt, self.health) = (0.0, 0.0, 0.0, 0.0, None);
    }

    /// Takes in shots from the server. Your own only set the markers (their tracers were drawn when you
    /// fired); the rest wait for the render clock.
    pub fn receive(&mut self, shots: &[ReceivedShot], own: Option<u32>) {
        for s in shots {
            if Some(s.shot.shooter) == own {
                if s.shot.kill {
                    self.kill = KILL_SECONDS;
                } else if s.shot.hit {
                    self.hit = HIT_SECONDS;
                }
            } else if self.pending.len() < MAX_PENDING {
                self.pending.push_back(*s);
            }
        }
    }

    /// Fires your tracers while `trigger` is held, at the rate the server fires. `soldier` is where your
    /// soldier is drawn, or `None` while it's dead or unknown.
    pub fn fire_own(&mut self, dt: f32, trigger: bool, soldier: Option<(f32, f32)>, yaw: f32, weapon: &Weapon, tick_rate: u32) {
        let Some((x, z)) = soldier.filter(|_| trigger) else {
            self.cooldown = (self.cooldown - dt).max(0.0);
            return;
        };
        let rate = tick_rate.max(1) as f32;
        let interval = (weapon.fire_interval * rate - 1e-3).ceil().max(1.0) / rate;
        self.cooldown -= dt;
        while self.cooldown <= 0.0 {
            self.cooldown += interval;
            self.launch(Vec3::new(x, CHEST_HEIGHT, z), Vec3::new(yaw.sin(), 0.0, yaw.cos()), weapon.range, OWN_TRACER);
        }
    }

    /// Launches the waiting shots that the render clock has reached, from where their soldiers are drawn.
    pub fn release(&mut self, render_tick: f64, views: &[EntityView], range: f32) {
        while let Some(&s) = self.pending.front() {
            let tick = f64::from(s.tick);
            if tick > render_tick {
                break;
            }
            self.pending.pop_front();
            if render_tick - tick <= STALE_TICKS {
                self.launch_shot(&s, tick, views, range);
            }
        }
    }

    fn launch_shot(&mut self, s: &ReceivedShot, tick: f64, views: &[EntityView], range: f32) {
        let shot = s.shot;
        let Some(shooter) = views.get(shot.shooter as usize) else { return };
        let Some((sx, sz, yaw)) = shooter.at(tick) else { return };
        let from = Vec2::new(sx, sz);
        let target = views
            .get(shot.target as usize)
            .filter(|_| shot.target != Shot::NO_TARGET)
            .and_then(|v| v.at(tick))
            .map(|(x, z, _)| Vec2::new(x, z));
        let (dir, length) = match target {
            Some(to) => {
                let line = to - from;
                let d = line.length();
                if d < 0.5 {
                    return;
                }
                if shot.hit {
                    (line / d, d)
                } else {
                    let u = rng::unit(u64::from(s.tick), u64::from(shot.shooter), shot.target as usize, SALT_MISS);
                    let side = if u < 0.5 { -1.0 } else { 1.0 };
                    let off = MISS_SIDE.0 + (u * 2.0).fract() * (MISS_SIDE.1 - MISS_SIDE.0);
                    let past = to + line.perp() / d * off * side - from;
                    (past.normalize(), past.length() + MISS_PAST)
                }
            }
            None => (Vec2::new(yaw.sin(), yaw.cos()), range),
        };
        let color = TEAM_TRACERS[shooter.team as usize & 3];
        self.launch(Vec3::new(sx, CHEST_HEIGHT, sz), Vec3::new(dir.x, 0.0, dir.y), length, color);
    }

    fn launch(&mut self, from: Vec3, dir: Vec3, length: f32, color: [f32; 3]) {
        if length > MUZZLE && self.flights.len() < MAX_FLIGHTS {
            self.flights.push(Flight { from: from + dir * MUZZLE, dir, length: length - MUZZLE, age: 0.0, color });
        }
    }

    /// Watches your soldier's health (`None` while unknown), flashing red when it drops. Dead counts as 0.
    pub fn track_health(&mut self, health: Option<f32>) {
        if let (Some(before), Some(now)) = (self.health, health)
            && now < before - 1e-3
        {
            self.hurt = HURT_SECONDS;
        }
        self.health = health;
    }

    /// Moves tracers along and runs down the markers.
    pub fn advance(&mut self, dt: f32) {
        for f in &mut self.flights {
            f.age += dt;
        }
        self.flights.retain(|f| f.age * TRACER_SPEED - TRACER_STREAK < f.length);
        self.hit = (self.hit - dt).max(0.0);
        self.kill = (self.kill - dt).max(0.0);
        self.hurt = (self.hurt - dt).max(0.0);
    }

    /// Every tracer in flight, as streaks to draw.
    pub fn tracers(&self, out: &mut Vec<Tracer>) {
        for f in &self.flights {
            let travelled = f.age * TRACER_SPEED;
            let (tail, head) = ((travelled - TRACER_STREAK).clamp(0.0, f.length), travelled.min(f.length));
            if head - tail < 0.01 {
                continue;
            }
            let (a, b) = (f.from + f.dir * tail, f.from + f.dir * head);
            out.push(Tracer { tail: [a.x, a.y, a.z, 0.0], head: [b.x, b.y, b.z, 0.0], color: [f.color[0], f.color[1], f.color[2], 1.0] });
        }
    }

    /// The screen overlay as triangles: hurt flash, crosshair (while `aiming`), then hit or kill marker.
    /// `size` is the drawing area in pixels and `scale` the display's scale factor.
    pub fn overlay(&self, out: &mut Vec<OverlayVertex>, size: (u32, u32), scale: f32, aiming: bool) {
        let screen = Screen { w: size.0.max(1) as f32, h: size.1.max(1) as f32, scale };
        if self.hurt > 0.0 {
            screen.edge_glow(out, HURT_COLOR, 0.45 * self.hurt / HURT_SECONDS);
        }
        if aiming {
            for dir in [Vec2::X, Vec2::NEG_X, Vec2::Y, Vec2::NEG_Y] {
                screen.stroke(out, dir * 5.0, dir * 14.0, 2.0, [1.0, 1.0, 1.0, 0.9]);
            }
        }
        let (color, alpha, reach) = if self.kill > 0.0 {
            (KILL_COLOR, self.kill / KILL_SECONDS, 22.0)
        } else {
            (HIT_COLOR, self.hit / HIT_SECONDS, 17.0)
        };
        if alpha > 0.0 {
            for dir in [Vec2::new(1.0, 1.0), Vec2::new(-1.0, 1.0), Vec2::new(1.0, -1.0), Vec2::new(-1.0, -1.0)] {
                let dir = dir.normalize();
                screen.stroke(out, dir * 8.0, dir * reach, 2.5, [color[0], color[1], color[2], alpha]);
            }
        }
    }
}

/// Converts pixel offsets from the screen centre (y up, before display scaling) into clip space.
struct Screen {
    w: f32,
    h: f32,
    scale: f32,
}

impl Screen {
    fn clip(&self, p: Vec2) -> [f32; 2] {
        [p.x * self.scale * 2.0 / self.w, p.y * self.scale * 2.0 / self.h]
    }

    fn quad(out: &mut Vec<OverlayVertex>, corners: [[f32; 2]; 4], colors: [[f32; 4]; 4]) {
        for k in [0, 1, 2, 0, 2, 3] {
            out.push(OverlayVertex { pos: corners[k], color: colors[k] });
        }
    }

    /// A line from `a` to `b`, `width` pixels wide, over a slightly larger dark outline so it shows on sky
    /// and ground alike.
    fn stroke(&self, out: &mut Vec<OverlayVertex>, a: Vec2, b: Vec2, width: f32, color: [f32; 4]) {
        let along = (b - a).normalize_or_zero();
        let mut line = |grow: f32, color: [f32; 4]| {
            let (a, b) = (a - along * grow, b + along * grow);
            let side = along.perp() * (width * 0.5 + grow);
            let corners = [self.clip(a - side), self.clip(b - side), self.clip(b + side), self.clip(a + side)];
            Screen::quad(out, corners, [color; 4]);
        };
        line(1.0, [0.0, 0.0, 0.0, color[3] * 0.45]);
        line(0.0, color);
    }

    /// A glow along all four edges of the screen, fading toward the middle.
    fn edge_glow(&self, out: &mut Vec<OverlayVertex>, rgb: [f32; 3], alpha: f32) {
        let band = 0.15 * self.w.min(self.h);
        let (bx, by) = (band * 2.0 / self.w, band * 2.0 / self.h);
        let (outer, inner) = ([rgb[0], rgb[1], rgb[2], alpha], [rgb[0], rgb[1], rgb[2], 0.0]);
        let o = [[-1.0, 1.0], [1.0, 1.0], [1.0, -1.0], [-1.0, -1.0]];
        let i = [[-1.0 + bx, 1.0 - by], [1.0 - bx, 1.0 - by], [1.0 - bx, -1.0 + by], [-1.0 + bx, -1.0 + by]];
        for k in 0..4 {
            let next = (k + 1) % 4;
            Screen::quad(out, [o[k], o[next], i[next], i[k]], [outer, outer, inner, inner]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horde_net::EntityState;

    const RIFLE: Weapon = Weapon { fire_interval: 0.12, range: 60.0 };

    fn soldier_at(x: f32, z: f32, team: u8, ticks: &[u32]) -> EntityView {
        let mut v = EntityView::default();
        for &t in ticks {
            v.push(t, &EntityState { x, z, yaw: 0.0, team, kind: 0, alive: true, health: 1.0 });
        }
        v
    }

    #[test]
    fn own_tracers_follow_the_servers_whole_tick_rate() {
        // 0.12 s at 30 Hz fires every 4th tick on the server: 8 shots in a second, not 9.
        let mut fx = Effects::default();
        for _ in 0..60 {
            fx.fire_own(1.0 / 60.0, true, Some((10.0, 10.0)), 0.0, &RIFLE, 30);
        }
        assert_eq!(fx.flights.len(), 8);
        // Letting go and pressing again fires at once.
        fx.fire_own(0.2, false, Some((10.0, 10.0)), 0.0, &RIFLE, 30);
        fx.fire_own(1.0 / 60.0, true, Some((10.0, 10.0)), 0.0, &RIFLE, 30);
        assert_eq!(fx.flights.len(), 9);
    }

    #[test]
    fn a_shot_waits_for_the_clock_then_flies_to_its_target() {
        let views = vec![soldier_at(0.0, 0.0, 1, &[9, 10]), soldier_at(0.0, 30.0, 0, &[9, 10])];
        let mut fx = Effects::default();
        let shot = Shot { shooter: 0, target: 1, hit: true, kill: false };
        fx.receive(&[ReceivedShot { tick: 10, shot }], Some(7));
        fx.release(9.5, &views, 60.0);
        assert!(fx.flights.is_empty(), "launched before its tick");
        fx.release(10.0, &views, 60.0);
        assert_eq!(fx.flights.len(), 1);
        let f = &fx.flights[0];
        let end = f.from + f.dir * f.length;
        assert!((end - Vec3::new(0.0, CHEST_HEIGHT, 30.0)).length() < 1e-3, "ends at {end}");

        // It's drawn as a streak no longer than TRACER_STREAK, and gone once its tail reaches the end.
        fx.advance(0.05);
        let mut out = Vec::new();
        fx.tracers(&mut out);
        let t = out[0];
        let len = Vec3::from_slice(&t.head[..3]).distance(Vec3::from_slice(&t.tail[..3]));
        assert!(len <= TRACER_STREAK + 1e-3);
        fx.advance(1.0);
        assert!(fx.flights.is_empty());
    }

    #[test]
    fn your_hits_and_kills_set_markers_without_a_second_tracer() {
        let mut fx = Effects::default();
        let hit = Shot { shooter: 3, target: 1, hit: true, kill: false };
        fx.receive(&[ReceivedShot { tick: 5, shot: hit }], Some(3));
        assert!(fx.hit > 0.0 && fx.kill == 0.0 && fx.pending.is_empty());
        fx.receive(&[ReceivedShot { tick: 6, shot: Shot { kill: true, ..hit } }], Some(3));
        assert!(fx.kill > 0.0);
    }

    #[test]
    fn losing_health_flashes_and_respawning_doesnt() {
        let mut fx = Effects::default();
        fx.track_health(Some(1.0));
        fx.track_health(Some(1.0));
        assert_eq!(fx.hurt, 0.0);
        fx.track_health(Some(0.66));
        assert!(fx.hurt > 0.0);
        fx.advance(1.0);
        fx.track_health(Some(1.0));
        assert_eq!(fx.hurt, 0.0);
    }
}
