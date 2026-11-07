//! Game client: a window onto a server's battle. It connects with [`NetClient`], smooths every
//! replicated entity, draws them, and sends the player's controls.
//!
//! Controls: click to capture the mouse, mouse to aim, WASD to move, left button to fire, V to switch
//! between first and third person, mouse wheel to zoom (third person), Esc to free the mouse.

mod camera;
mod effects;
mod render;
mod view;

pub use effects::Weapon;

use camera::{Camera, Mode};
use effects::Effects;
use horde_cpu::{CpuPlan, Placement, Topology};
use horde_net::{NetClient, PlayerInput};
use render::{CameraUniform, Instance, OverlayVertex, Renderer, Tracer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use view::{Clock, EntityView, OWN_DELAY_TICKS, REMOTE_DELAY_TICKS};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

const MOUSE_SENSITIVITY: f32 = 0.0022;
/// Controls go to the server at least this often, and on every frame up to 60 a second.
const INPUT_INTERVAL: Duration = Duration::from_micros(16_600);
const SOLDIER_HEIGHT: f32 = 1.8;
const OWN_COLOR: [f32; 3] = [0.95, 0.78, 0.25];
const TEAM_COLORS: [[f32; 3]; 4] = [[0.25, 0.45, 0.95], [0.85, 0.25, 0.20], [0.3, 0.8, 0.35], [0.7, 0.7, 0.7]];

pub struct ClientConfig {
    pub server: SocketAddr,
    pub name: String,
    /// Window title prefix, usually the game's name.
    pub title: String,
    /// Keep this process off the cores a server on the same PC would use (see `horde-cpu`).
    pub stay_off_server_cores: bool,
    /// The player's weapon, so the client can draw its tracers the moment it fires.
    pub weapon: Weapon,
}

/// Opens the window and runs until it's closed.
pub fn run(config: ClientConfig) -> Result<(), String> {
    if config.stay_off_server_cores {
        let topology = Topology::detect();
        let plan = CpuPlan::new(&topology, Placement::SharedWithClient);
        horde_cpu::restrict_process(&plan.client_cpus);
    }
    let net = NetClient::connect(config.server, &config.name).map_err(|e| format!("can't open a socket: {e}"))?;
    let event_loop = EventLoop::new().map_err(|e| format!("can't start the window system: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(config, net);
    event_loop.run_app(&mut app).map_err(|e| format!("window system error: {e}"))?;
    app.error.map_or(Ok(()), Err)
}

#[derive(Default)]
struct Keys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
}

struct Stats {
    since: Instant,
    frames: u32,
    bytes: u64,
}

struct App {
    config: ClientConfig,
    net: NetClient,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    error: Option<String>,
    views: Vec<EntityView>,
    clock: Clock,
    /// The connection the views belong to; a new one (server restarted) starts them over.
    session: u32,
    camera: Camera,
    keys: Keys,
    firing: bool,
    captured: bool,
    last_frame: Instant,
    last_input: Instant,
    effects: Effects,
    /// Per-frame draw lists, refilled every frame.
    instances: Vec<Instance>,
    tracers: Vec<Tracer>,
    overlay: Vec<OverlayVertex>,
    stats: Stats,
}

impl App {
    fn new(config: ClientConfig, net: NetClient) -> App {
        let now = Instant::now();
        App {
            config,
            net,
            window: None,
            renderer: None,
            error: None,
            views: Vec::new(),
            clock: Clock::default(),
            session: 0,
            camera: Camera::default(),
            keys: Keys::default(),
            firing: false,
            captured: false,
            last_frame: now,
            last_input: now,
            effects: Effects::default(),
            instances: Vec::new(),
            tracers: Vec::new(),
            overlay: Vec::new(),
            stats: Stats { since: now, frames: 0, bytes: 0 },
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, message: String) {
        self.error = Some(message);
        event_loop.exit();
    }

    fn set_captured(&mut self, captured: bool) {
        let Some(window) = &self.window else { return };
        if captured {
            let grabbed = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            window.set_cursor_visible(!grabbed);
            self.captured = grabbed;
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.captured = false;
            self.firing = false;
        }
    }

    /// The kind of entity the player controls, for per-kind camera preferences.
    fn own_kind(&self) -> u8 {
        self.net.entity().and_then(|e| self.views.get(e as usize)).map_or(0, |v| v.kind)
    }

    fn frame(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f64().min(0.25);
        self.last_frame = now;

        // Take in everything the server sent.
        let world = self.net.world_size();
        let (views, clock) = (&mut self.views, &mut self.clock);
        self.net.poll(|entity, state, _previous, tick| {
            let e = entity as usize;
            if e >= views.len() {
                views.resize(e + 1, EntityView::default());
            }
            views[e].push(tick, &state.to_state(world));
            clock.on_tick(tick);
        });
        if self.net.session() != self.session {
            // Joined a new game (first connection, or the server restarted): its ticks start over.
            self.session = self.net.session();
            self.views.clear();
            self.clock = Clock::default();
            self.effects.clear();
        }
        self.effects.receive(self.net.shots(), self.net.entity());
        self.clock.advance(dt, f64::from(self.net.tick_rate().max(1)));

        // Send controls.
        if now - self.last_input >= INPUT_INTERVAL {
            self.last_input = now;
            let ((fx, fz), (rx, rz)) = self.camera.ground_axes();
            let ahead = f32::from(u8::from(self.keys.forward)) - f32::from(u8::from(self.keys.back));
            let side = f32::from(u8::from(self.keys.right)) - f32::from(u8::from(self.keys.left));
            self.net.send_input(PlayerInput {
                move_x: fx * ahead + rx * side,
                move_z: fz * ahead + rz * side,
                yaw: self.camera.yaw,
                fire: self.firing && self.captured,
            });
        }

        let Some(renderer) = self.renderer.as_mut() else { return Ok(()) };
        let own = self.net.entity().map(|e| e as usize);
        let kind = own.and_then(|e| self.views.get(e)).map_or(0, |v| v.kind);
        let first_person = self.camera.mode(kind) == Mode::FirstPerson;

        // Every living soldier, drawn a few ticks in the past and blended between updates.
        let remote_tick = self.clock.now() - REMOTE_DELAY_TICKS;
        self.instances.clear();
        let mut alive = 0usize;
        for (e, v) in self.views.iter().enumerate() {
            if !v.alive || !v.is_known() {
                continue;
            }
            alive += 1;
            let is_own = Some(e) == own;
            if is_own && first_person {
                continue;
            }
            let tick = if is_own { self.clock.now() - OWN_DELAY_TICKS } else { remote_tick };
            let Some((x, z, yaw)) = v.at(tick) else { continue };
            let color = if is_own { OWN_COLOR } else { TEAM_COLORS[v.team as usize & 3] };
            let shade = 0.55 + 0.45 * v.health;
            self.instances.push(Instance {
                place: [x, z, yaw, SOLDIER_HEIGHT],
                color: [color[0] * shade, color[1] * shade, color[2] * shade, 1.0],
            });
        }

        // The camera follows the player's soldier, or looks over the middle of the map until it exists.
        let own_view = own.and_then(|e| self.views.get(e)).filter(|v| v.is_known());
        let own_place = own_view.and_then(|v| v.at(self.clock.now() - OWN_DELAY_TICKS)).map(|(x, z, _)| (x, z));
        let (px, pz) = own_place.unwrap_or((world * 0.5, world * 0.5));
        let (eye, view_proj) = self.camera.view(kind, px, pz, renderer.aspect());

        // Shots: yours leave as you fire, everyone else's when the drawn battle reaches them.
        let own_alive = own_view.is_some_and(|v| v.alive);
        let weapon = self.config.weapon;
        let trigger = self.firing && self.captured;
        let soldier = own_place.filter(|_| own_alive);
        self.effects.fire_own(dt as f32, trigger, soldier, self.camera.yaw, &weapon, self.net.tick_rate());
        self.effects.release(remote_tick, &self.views, weapon.range);
        self.effects.track_health(own_view.map(|v| if v.alive { v.health } else { 0.0 }));
        self.effects.advance(dt as f32);
        self.tracers.clear();
        self.effects.tracers(&mut self.tracers);
        self.overlay.clear();
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        self.effects.overlay(&mut self.overlay, renderer.size(), scale, self.captured);
        let sun = glam::Vec3::new(0.4, 0.8, 0.3).normalize();
        let uniform = CameraUniform {
            view_proj: view_proj.to_cols_array_2d(),
            eye: [eye.x, eye.y, eye.z, world],
            sun: [sun.x, sun.y, sun.z, 0.0],
            fog: [0.55, 0.66, 0.78, 0.0012],
        };
        renderer.render(&uniform, &self.instances, &self.tracers, &self.overlay)?;

        // Once a second, put the numbers in the title bar.
        self.stats.frames += 1;
        let elapsed = self.stats.since.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let (bytes, _) = self.net.traffic();
            let health = own.and_then(|e| self.views.get(e)).map_or(0.0, |v| v.health * 100.0);
            let status = if !self.net.is_connected() {
                format!("connecting to {}...", self.config.server)
            } else if !self.captured {
                "click to play".to_string()
            } else {
                format!(
                    "{} view (V), WASD, mouse aims, click fires, Esc frees mouse",
                    if first_person { "1st-person" } else { "3rd-person" }
                )
            };
            if let Some(window) = &self.window {
                window.set_title(&format!(
                    "{}  |  {:.0} fps  |  {} soldiers  |  {:.0} KB/s  |  health {:.0}%  |  {}",
                    self.config.title,
                    f64::from(self.stats.frames) / elapsed,
                    alive,
                    (bytes - self.stats.bytes) as f64 / elapsed / 1024.0,
                    health,
                    status,
                ));
            }
            self.stats = Stats { since: Instant::now(), frames: 0, bytes };
        }
        Ok(())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes =
            Window::default_attributes().with_title(&self.config.title).with_inner_size(LogicalSize::new(1600.0, 900.0));
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("can't open a window: {e}")),
        };
        match Renderer::new(window.clone()) {
            Ok(renderer) => {
                println!("rendering with {}", renderer.adapter_name);
                self.renderer = Some(renderer);
            }
            Err(e) => return self.fail(event_loop, e),
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::Focused(false) => {
                self.keys = Keys::default();
                self.set_captured(false);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let PhysicalKey::Code(code) = event.physical_key else { return };
                match code {
                    KeyCode::KeyW => self.keys.forward = pressed,
                    KeyCode::KeyS => self.keys.back = pressed,
                    KeyCode::KeyA => self.keys.left = pressed,
                    KeyCode::KeyD => self.keys.right = pressed,
                    KeyCode::KeyV if pressed && !event.repeat => {
                        let kind = self.own_kind();
                        self.camera.toggle_mode(kind);
                    }
                    KeyCode::Escape if pressed => self.set_captured(false),
                    _ => {}
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Pressed && !self.captured {
                    self.set_captured(true);
                } else {
                    self.firing = state == ElementState::Pressed;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                let kind = self.own_kind();
                self.camera.zoom(kind, steps);
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.frame() {
                    self.fail(event_loop, e);
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.captured
        {
            self.camera.turn(delta.0 as f32 * MOUSE_SENSITIVITY, delta.1 as f32 * MOUSE_SENSITIVITY);
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}
