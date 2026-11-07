//! The renderer: DirectX 12 through wgpu. One draw for the ground, one instanced draw for every soldier,
//! one for every tracer, and one for the screen overlay (crosshair, markers).

use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SKY: wgpu::Color = wgpu::Color { r: 0.55, g: 0.66, b: 0.78, a: 1.0 };
/// Light added on top of what's behind: tracers glow instead of covering.
const ADDITIVE: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::SrcAlpha,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    /// xyz eye position, w world size.
    pub eye: [f32; 4],
    /// xyz direction toward the sun.
    pub sun: [f32; 4],
    /// rgb fog colour, a density.
    pub fog: [f32; 4],
}

/// One soldier to draw.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    /// x, z, yaw, height.
    pub place: [f32; 4],
    /// rgb, unused.
    pub color: [f32; 4],
}

/// One tracer streak to draw, as a camera-facing ribbon from its tail (faint) to its head (bright).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Tracer {
    /// xyz, unused.
    pub tail: [f32; 4],
    /// xyz, unused.
    pub head: [f32; 4],
    /// rgb, brightness.
    pub color: [f32; 4],
}

/// A corner of a screen-space triangle: position in clip space (-1 to 1, y up) and colour with alpha.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct OverlayVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4];
const TRACER_ATTRIBUTES: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
const OVERLAY_ATTRIBUTES: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

/// A vertex buffer refilled every frame, grown when a frame needs more room than it has.
struct Stream {
    label: &'static str,
    buffer: wgpu::Buffer,
    capacity: usize,
    len: u32,
}

impl Stream {
    fn new<T>(device: &wgpu::Device, label: &'static str, capacity: usize) -> Stream {
        Stream { label, buffer: vertex_buffer(device, label, capacity * size_of::<T>()), capacity, len: 0 }
    }

    fn upload<T: Pod>(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, items: &[T]) {
        if items.len() > self.capacity {
            self.capacity = items.len().next_power_of_two();
            self.buffer = vertex_buffer(device, self.label, self.capacity * size_of::<T>());
        }
        if !items.is_empty() {
            queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(items));
        }
        self.len = items.len() as u32;
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    camera: wgpu::Buffer,
    camera_bind: wgpu::BindGroup,
    ground: wgpu::RenderPipeline,
    soldier: wgpu::RenderPipeline,
    tracer: wgpu::RenderPipeline,
    overlay: wgpu::RenderPipeline,
    cube: wgpu::Buffer,
    cube_vertices: u32,
    soldiers: Stream,
    tracers: Stream,
    overlay_vertices: Stream,
    pub adapter_name: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Renderer, String> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance.create_surface(window).map_err(|e| format!("can't create a drawing surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no suitable GPU: {e}"))?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("horde"),
            ..Default::default()
        }))
        .map_err(|e| format!("can't open the GPU: {e}"))?;

        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the GPU can't draw to this window")?;
        // Vsync: the frame rate follows the display, and the client doesn't burn CPU or GPU on frames
        // nobody sees.
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);
        let depth = create_depth(&device, &config);

        let camera = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let camera_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: camera.as_entire_binding() }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });

        // Solid things write depth; tracers and the overlay are blended on top without writing it.
        let pipeline = |label: &str,
                        vs: &str,
                        fs: &str,
                        buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
                        blend: wgpu::BlendState,
                        depth_write: bool,
                        depth_compare: wgpu::CompareFunction| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(depth_compare),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: config.format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let solid = (wgpu::BlendState::REPLACE, true, wgpu::CompareFunction::Less);
        let ground = pipeline("ground", "vs_ground", "fs_ground", &[], solid.0, solid.1, solid.2);
        let soldier = pipeline(
            "soldier",
            "vs_soldier",
            "fs_soldier",
            &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VERTEX_ATTRIBUTES,
                }),
                Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &INSTANCE_ATTRIBUTES,
                }),
            ],
            solid.0,
            solid.1,
            solid.2,
        );
        let tracer = pipeline(
            "tracer",
            "vs_tracer",
            "fs_tracer",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<Tracer>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &TRACER_ATTRIBUTES,
            })],
            ADDITIVE,
            false,
            wgpu::CompareFunction::Less,
        );
        let overlay = pipeline(
            "overlay",
            "vs_overlay",
            "fs_overlay",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<OverlayVertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &OVERLAY_ATTRIBUTES,
            })],
            wgpu::BlendState::ALPHA_BLENDING,
            false,
            wgpu::CompareFunction::Always,
        );

        let vertices = cube_vertices();
        let cube = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cube"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let soldiers = Stream::new::<Instance>(&device, "soldiers", 1024);
        let tracers = Stream::new::<Tracer>(&device, "tracers", 1024);
        let overlay_vertices = Stream::new::<OverlayVertex>(&device, "overlay", 256);

        Ok(Renderer {
            surface,
            device,
            queue,
            config,
            depth,
            camera,
            camera_bind,
            ground,
            soldier,
            tracer,
            overlay,
            cube,
            cube_vertices: vertices.len() as u32,
            soldiers,
            tracers,
            overlay_vertices,
            adapter_name,
        })
    }

    pub fn aspect(&self) -> f32 {
        self.config.width as f32 / self.config.height.max(1) as f32
    }

    /// Size of the drawing area in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.depth = create_depth(&self.device, &self.config);
    }

    pub fn render(
        &mut self,
        camera: &CameraUniform,
        soldiers: &[Instance],
        tracers: &[Tracer],
        overlay: &[OverlayVertex],
    ) -> Result<(), String> {
        let (frame, reconfigure) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Validation => return Err("the GPU rejected the frame".into()),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.queue.write_buffer(&self.camera, 0, bytemuck::bytes_of(camera));
        self.soldiers.upload(&self.device, &self.queue, soldiers);
        self.tracers.upload(&self.device, &self.queue, tracers);
        self.overlay_vertices.upload(&self.device, &self.queue, overlay);

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(SKY), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.camera_bind, &[]);
            pass.set_pipeline(&self.ground);
            pass.draw(0..6, 0..1);
            if self.soldiers.len > 0 {
                pass.set_pipeline(&self.soldier);
                pass.set_vertex_buffer(0, self.cube.slice(..));
                pass.set_vertex_buffer(1, self.soldiers.buffer.slice(..));
                pass.draw(0..self.cube_vertices, 0..self.soldiers.len);
            }
            if self.tracers.len > 0 {
                pass.set_pipeline(&self.tracer);
                pass.set_vertex_buffer(0, self.tracers.buffer.slice(..));
                pass.draw(0..6, 0..self.tracers.len);
            }
            if self.overlay_vertices.len > 0 {
                pass.set_pipeline(&self.overlay);
                pass.set_vertex_buffer(0, self.overlay_vertices.buffer.slice(..));
                pass.draw(0..self.overlay_vertices.len, 0..1);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        if reconfigure {
            self.surface.configure(&self.device, &self.config);
        }
        Ok(())
    }
}

fn create_depth(device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: config.width, height: config.height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn vertex_buffer(device: &wgpu::Device, label: &str, bytes: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// A box from (-0.5, 0, -0.5) to (0.5, 1, 0.5): feet on the ground, scaled per soldier in the shader.
fn cube_vertices() -> Vec<Vertex> {
    // Each face: its normal, and two axes spanning it.
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, 0.0, 1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut out = Vec::with_capacity(36);
    for (n, u, v) in faces {
        let corner = |a: f32, b: f32| {
            let p = [0.5 * n[0] + a * u[0] + b * v[0], 0.5 * n[1] + a * u[1] + b * v[1], 0.5 * n[2] + a * u[2] + b * v[2]];
            Vertex { pos: [p[0], p[1] + 0.5, p[2]], normal: n }
        };
        let (a, b, c, d) = (corner(-0.5, -0.5), corner(0.5, -0.5), corner(0.5, 0.5), corner(-0.5, 0.5));
        out.extend_from_slice(&[a, b, c, a, c, d]);
    }
    out
}
