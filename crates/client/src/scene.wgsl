// Scene shaders: an infinite-looking ground grid and instanced soldier blocks, lit by one sun, with
// distance fog; glowing tracer ribbons; and a flat screen overlay.

struct Camera {
    view_proj: mat4x4<f32>,
    // xyz: eye position; w: world size in metres.
    eye: vec4<f32>,
    // xyz: direction toward the sun.
    sun: vec4<f32>,
    // rgb: fog colour; a: fog density.
    fog: vec4<f32>,
};

@group(0) @binding(0) var<uniform> cam: Camera;

// How much of what's at `world` the fog hides, 0 to 1.
fn fog_amount(world: vec3<f32>) -> f32 {
    let d = distance(world, cam.eye.xyz);
    return clamp(1.0 - exp(-d * cam.fog.a), 0.0, 1.0);
}

fn apply_fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> {
    return mix(color, cam.fog.rgb, fog_amount(world));
}

// ---- Ground ---------------------------------------------------------------------------------------

struct GroundOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_ground(@builtin(vertex_index) i: u32) -> GroundOut {
    // Two triangles covering the world square, extended a little past its edges.
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
        vec2(0.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0),
    );
    let size = cam.eye.w;
    let xz = (corners[i] * 1.2 - 0.1) * size;
    var out: GroundOut;
    out.world = vec3(xz.x, 0.0, xz.y);
    out.clip = cam.view_proj * vec4(out.world, 1.0);
    return out;
}

fn grid_line(coord: vec2<f32>) -> f32 {
    let g = abs(fract(coord - 0.5) - 0.5) / fwidth(coord);
    return 1.0 - min(min(g.x, g.y), 1.0);
}

@fragment
fn fs_ground(v: GroundOut) -> @location(0) vec4<f32> {
    let size = cam.eye.w;
    let inside = v.world.x >= 0.0 && v.world.z >= 0.0 && v.world.x <= size && v.world.z <= size;
    var color = select(vec3(0.20, 0.22, 0.19), vec3(0.30, 0.36, 0.26), inside);
    color = mix(color, color * 0.72, grid_line(v.world.xz / 10.0));
    color = mix(color, color * 0.45, grid_line(v.world.xz / 100.0));
    return vec4(apply_fog(color, v.world), 1.0);
}

// ---- Soldiers -------------------------------------------------------------------------------------

struct SoldierOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) front: f32,
};

@vertex
fn vs_soldier(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    // x, z, yaw, height
    @location(2) place: vec4<f32>,
    // rgb, unused
    @location(3) tint: vec4<f32>,
) -> SoldierOut {
    let c = cos(place.z);
    let s = sin(place.z);
    let p = pos * vec3(0.6, place.w, 0.45);
    // Yaw turns local +z (forward) toward +x.
    let world = vec3(place.x + p.x * c + p.z * s, p.y, place.y - p.x * s + p.z * c);
    var out: SoldierOut;
    out.clip = cam.view_proj * vec4(world, 1.0);
    out.world = world;
    out.normal = vec3(normal.x * c + normal.z * s, normal.y, -normal.x * s + normal.z * c);
    out.color = tint.rgb;
    out.front = select(0.0, 1.0, normal.z > 0.5);
    return out;
}

@fragment
fn fs_soldier(v: SoldierOut) -> @location(0) vec4<f32> {
    let light = 0.35 + 0.65 * max(dot(normalize(v.normal), cam.sun.xyz), 0.0);
    var color = v.color * light;
    // A brighter front face shows which way the soldier faces.
    color = mix(color, min(color * 1.7 + vec3(0.12), vec3(1.0)), v.front);
    return vec4(apply_fog(color, v.world), 1.0);
}

// ---- Tracers --------------------------------------------------------------------------------------

struct TracerOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) color: vec4<f32>,
    // x: 0 at the tail to 1 at the head; y: -1 to 1 across the ribbon.
    @location(2) along: vec2<f32>,
};

@vertex
fn vs_tracer(
    @builtin(vertex_index) i: u32,
    @location(0) tail: vec4<f32>,
    @location(1) head: vec4<f32>,
    // rgb, brightness
    @location(2) tint: vec4<f32>,
) -> TracerOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, -1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
        vec2(0.0, -1.0), vec2(1.0, 1.0), vec2(0.0, 1.0),
    );
    let c = corners[i];
    let p = mix(tail.xyz, head.xyz, c.x);
    let to_eye = cam.eye.xyz - p;
    // A ribbon turned to face the camera, at least about two pixels wide however far away it is.
    let across = cross(head.xyz - tail.xyz, to_eye);
    let n = length(across);
    let side = select(vec3(0.0, 1.0, 0.0), across / n, n > 1e-6);
    let half_width = max(0.03, length(to_eye) * 0.0009);
    let world = p + side * half_width * c.y;
    var out: TracerOut;
    out.clip = cam.view_proj * vec4(world, 1.0);
    out.world = world;
    out.color = tint;
    out.along = c;
    return out;
}

@fragment
fn fs_tracer(v: TracerOut) -> @location(0) vec4<f32> {
    // Brightest at the head and along the middle, fading toward the tail and the edges, and into the fog.
    let edge = 1.0 - v.along.y * v.along.y;
    let glow = smoothstep(0.0, 0.7, v.along.x) * edge * v.color.a * (1.0 - fog_amount(v.world));
    return vec4(v.color.rgb, glow);
}

// ---- Overlay --------------------------------------------------------------------------------------

struct OverlayOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_overlay(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> OverlayOut {
    var out: OverlayOut;
    out.clip = vec4(pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_overlay(v: OverlayOut) -> @location(0) vec4<f32> {
    return v.color;
}
