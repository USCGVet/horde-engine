//! Smoothing replicated entities: a client clock that tracks the server's tick, and per-entity sample
//! histories that are interpolated a little in the past so movement looks continuous between updates.

use horde_net::EntityState;
use std::time::Instant;

/// How far behind the server clock other entities are drawn, in ticks: enough that even an entity
/// updated only every other tick has a later sample to blend toward.
pub const REMOTE_DELAY_TICKS: f64 = 3.0;
/// The player's own entity updates every tick, so it needs only one tick of delay.
pub const OWN_DELAY_TICKS: f64 = 1.0;
/// Past the newest sample, keep moving along the last velocity for at most this many ticks.
const MAX_EXTRAPOLATE_TICKS: f64 = 2.0;
/// A jump bigger than this between samples is a respawn or teleport: snap instead of sliding.
const TELEPORT_METRES: f32 = 15.0;

/// Estimates the server's current tick as a continuous value, smoothed against arrival jitter.
#[derive(Default)]
pub struct Clock {
    estimate: f64,
    latest: u32,
    latest_at: Option<Instant>,
}

impl Clock {
    pub fn on_tick(&mut self, tick: u32) {
        if tick > self.latest {
            self.latest = tick;
            self.latest_at = Some(Instant::now());
        }
    }

    pub fn advance(&mut self, dt: f64, tick_rate: f64) {
        let Some(at) = self.latest_at else { return };
        let target = f64::from(self.latest) + at.elapsed().as_secs_f64() * tick_rate;
        self.estimate += dt * tick_rate;
        let error = target - self.estimate;
        if error.abs() > 5.0 {
            self.estimate = target;
        } else {
            self.estimate += error * 0.05;
        }
    }

    /// The smoothed server tick right now.
    pub fn now(&self) -> f64 {
        self.estimate
    }
}

#[derive(Clone, Copy, Default)]
struct Sample {
    tick: f64,
    x: f32,
    z: f32,
    yaw: f32,
}

/// The last few states received for one entity, newest first.
#[derive(Clone, Default)]
pub struct EntityView {
    samples: [Sample; 4],
    len: usize,
    pub team: u8,
    pub kind: u8,
    pub alive: bool,
    pub health: f32,
}

impl EntityView {
    pub fn push(&mut self, tick: u32, state: &EntityState) {
        self.samples.copy_within(0..3, 1);
        self.samples[0] = Sample { tick: f64::from(tick), x: state.x, z: state.z, yaw: state.yaw };
        self.len = (self.len + 1).min(self.samples.len());
        self.team = state.team;
        self.kind = state.kind;
        self.alive = state.alive;
        self.health = state.health;
    }

    pub fn is_known(&self) -> bool {
        self.len > 0
    }

    /// Position (x, z) and facing at server time `tick`, blended between the samples around it.
    pub fn at(&self, tick: f64) -> Option<(f32, f32, f32)> {
        if self.len == 0 {
            return None;
        }
        let newest = self.samples[0];
        if tick >= newest.tick {
            if self.len >= 2 {
                let previous = self.samples[1];
                let span = newest.tick - previous.tick;
                if span > 0.0 && !teleported(&previous, &newest) {
                    let ahead = (tick - newest.tick).min(MAX_EXTRAPOLATE_TICKS);
                    return Some(blend(&previous, &newest, (1.0 + ahead / span) as f32));
                }
            }
            return Some((newest.x, newest.z, newest.yaw));
        }
        for i in 0..self.len - 1 {
            let (later, earlier) = (self.samples[i], self.samples[i + 1]);
            if tick >= earlier.tick {
                if teleported(&earlier, &later) {
                    return Some((later.x, later.z, later.yaw));
                }
                let t = ((tick - earlier.tick) / (later.tick - earlier.tick).max(1e-6)) as f32;
                return Some(blend(&earlier, &later, t));
            }
        }
        let oldest = self.samples[self.len - 1];
        Some((oldest.x, oldest.z, oldest.yaw))
    }
}

fn teleported(a: &Sample, b: &Sample) -> bool {
    (b.x - a.x).powi(2) + (b.z - a.z).powi(2) > TELEPORT_METRES * TELEPORT_METRES
}

/// Linear blend of position and shortest-way blend of facing; `t` past 1 extrapolates.
fn blend(a: &Sample, b: &Sample, t: f32) -> (f32, f32, f32) {
    use std::f32::consts::{PI, TAU};
    let turn = (b.yaw - a.yaw + PI).rem_euclid(TAU) - PI;
    (a.x + (b.x - a.x) * t, a.z + (b.z - a.z) * t, a.yaw + turn * t.min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(x: f32, z: f32, yaw: f32) -> EntityState {
        EntityState { x, z, yaw, alive: true, health: 1.0, ..Default::default() }
    }

    #[test]
    fn interpolates_between_samples() {
        let mut v = EntityView::default();
        v.push(10, &state(0.0, 0.0, 0.0));
        v.push(12, &state(2.0, 4.0, 1.0));
        let (x, z, yaw) = v.at(11.0).unwrap();
        assert!((x - 1.0).abs() < 1e-5 && (z - 2.0).abs() < 1e-5 && (yaw - 0.5).abs() < 1e-5);
    }

    #[test]
    fn extrapolates_briefly_then_holds() {
        let mut v = EntityView::default();
        v.push(10, &state(0.0, 0.0, 0.0));
        v.push(11, &state(1.0, 0.0, 0.0));
        assert!((v.at(12.0).unwrap().0 - 2.0).abs() < 1e-5);
        assert!((v.at(20.0).unwrap().0 - 3.0).abs() < 1e-5, "capped at two ticks ahead");
    }

    #[test]
    fn snaps_across_a_respawn() {
        let mut v = EntityView::default();
        v.push(10, &state(0.0, 0.0, 0.0));
        v.push(11, &state(500.0, 0.0, 0.0));
        assert_eq!(v.at(10.5).unwrap().0, 500.0);
    }

    #[test]
    fn turns_the_short_way() {
        let mut v = EntityView::default();
        v.push(10, &state(0.0, 0.0, 6.0));
        v.push(12, &state(0.0, 0.0, 0.2));
        let yaw = v.at(11.0).unwrap().2;
        assert!((yaw - 6.0 - 0.2415).abs() < 0.01, "went the long way: {yaw}");
    }
}
