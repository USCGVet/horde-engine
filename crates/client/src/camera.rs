//! The player's camera: first or third person, switched with V. Each kind of controlled entity remembers
//! its own mode and zoom, so a soldier and (later) a vehicle can each keep the view the player prefers.

// The world is left-handed: x right, y up, z forward, so facing +z puts +x on the right of the screen.
// (The simulation's yaw turns from +z toward +x, which is turning right.) Right-handed matrices would
// mirror the view left to right.
use glam::camera::lh::{proj::directx, view::look_to_mat4};
use glam::{Mat4, Vec3};

const EYE_HEIGHT: f32 = 1.6;
const MIN_PITCH: f32 = -1.45;
const MAX_PITCH: f32 = 1.45;
const MIN_DISTANCE: f32 = 2.0;
const MAX_DISTANCE: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    FirstPerson,
    ThirdPerson,
}

#[derive(Clone, Copy)]
struct Preference {
    mode: Mode,
    distance: f32,
}

impl Default for Preference {
    fn default() -> Preference {
        Preference { mode: Mode::ThirdPerson, distance: 6.0 }
    }
}

pub struct Camera {
    /// Facing in radians: 0 looks along +z, positive turns toward +x. Also the player's aim.
    pub yaw: f32,
    /// Up is positive.
    pub pitch: f32,
    /// One preference per entity kind (0–31).
    preferences: [Preference; 32],
}

impl Default for Camera {
    fn default() -> Camera {
        Camera { yaw: 0.0, pitch: -0.15, preferences: [Preference::default(); 32] }
    }
}

impl Camera {
    pub fn mode(&self, kind: u8) -> Mode {
        self.preferences[kind as usize & 31].mode
    }

    pub fn toggle_mode(&mut self, kind: u8) {
        let p = &mut self.preferences[kind as usize & 31];
        p.mode = if p.mode == Mode::FirstPerson { Mode::ThirdPerson } else { Mode::FirstPerson };
    }

    /// Mouse wheel: closer for positive steps. Only affects third person.
    pub fn zoom(&mut self, kind: u8, steps: f32) {
        let p = &mut self.preferences[kind as usize & 31];
        p.distance = (p.distance * 0.85f32.powf(steps)).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    pub fn turn(&mut self, dx: f32, dy: f32) {
        self.yaw = (self.yaw + dx).rem_euclid(std::f32::consts::TAU);
        self.pitch = (self.pitch - dy).clamp(MIN_PITCH, MAX_PITCH);
    }

    /// Flat forward and right vectors for movement, as (x, z).
    pub fn ground_axes(&self) -> ((f32, f32), (f32, f32)) {
        let (s, c) = self.yaw.sin_cos();
        ((s, c), (c, -s))
    }

    /// Eye position and view-projection matrix for an entity standing at (x, z).
    pub fn view(&self, kind: u8, x: f32, z: f32, aspect: f32) -> (Vec3, Mat4) {
        let look = Vec3::new(self.yaw.sin() * self.pitch.cos(), self.pitch.sin(), self.yaw.cos() * self.pitch.cos());
        let head = Vec3::new(x, EYE_HEIGHT, z);
        let p = self.preferences[kind as usize & 31];
        let eye = match p.mode {
            Mode::FirstPerson => head,
            Mode::ThirdPerson => {
                // Behind and slightly above the shoulder; never below the ground.
                let mut eye = head - look * p.distance + Vec3::Y * 0.6;
                eye.y = eye.y.max(0.4);
                eye
            }
        };
        let near = if p.mode == Mode::FirstPerson { 0.05 } else { 0.1 };
        // wgpu uses DirectX's clip space: depth 0 to 1, y up.
        let proj = directx::perspective(70f32.to_radians(), aspect, near, 6000.0);
        let view = look_to_mat4(eye, look, Vec3::Y);
        (eye, proj * view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec4;

    fn ndc(m: Mat4, p: Vec3) -> Vec3 {
        let c = m * Vec4::new(p.x, p.y, p.z, 1.0);
        Vec3::new(c.x / c.w, c.y / c.w, c.z / c.w)
    }

    #[test]
    fn third_person_sees_own_soldier_in_front_and_horizon_above_centre() {
        let camera = Camera::default();
        let (_, m) = camera.view(0, 500.0, 500.0, 16.0 / 9.0);
        let body = ndc(m, Vec3::new(500.0, 0.9, 500.0));
        println!("own soldier at ndc {body:?}");
        assert!(body.x.abs() < 0.2 && body.y.abs() < 1.0 && (0.0..1.0).contains(&body.z), "{body:?}");
        let horizon = ndc(m, Vec3::new(500.0, 3.0, 5000.0));
        println!("horizon at ndc {horizon:?}");
        assert!(horizon.y > 0.0, "looking slightly down, the horizon should be above centre: {horizon:?}");
    }

    #[test]
    fn right_is_right() {
        // Facing +z (yaw 0), a point toward +x must be on the right of the screen...
        let mut camera = Camera::default();
        let (_, m) = camera.view(0, 500.0, 500.0, 16.0 / 9.0);
        assert!(ndc(m, Vec3::new(510.0, 1.0, 530.0)).x > 0.0);
        // ...the movement "right" axis must point there too...
        let (_, (rx, rz)) = camera.ground_axes();
        assert!(rx > 0.9 && rz.abs() < 1e-6);
        // ...and moving the mouse right (positive dx) must turn right, toward +x.
        camera.turn(0.3, 0.0);
        let ((fx, _), _) = camera.ground_axes();
        assert!(fx > 0.0);
        let (_, m) = camera.view(0, 500.0, 500.0, 16.0 / 9.0);
        assert!(ndc(m, Vec3::new(500.0 + 30.0 * 0.3f32.sin(), 1.0, 500.0 + 30.0 * 0.3f32.cos())).x.abs() < 0.1);
    }
}
