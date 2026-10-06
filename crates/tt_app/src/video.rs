//! Video display (DESIGN §13 step 5): NV12 planes uploaded to two wgpu textures
//! and converted to RGB in a fragment shader, drawn through an egui_wgpu paint
//! callback. The panel maps panel coordinates to video UVs with a uniform, so
//! zoom and pan (and later, derived views) are just a different mapping.
//!
//! A second draw per frame may repeat the video inside a circle (the clear
//! window around the pointer while sketching, painted over the overlays): it
//! has its own uniforms, and discards everything outside the circle.

use std::sync::atomic::{AtomicU32, Ordering};

use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState};
use eframe::wgpu;
use tt_media::{ColorInfo, FrameData, Matrix};

/// CPU time of the last texture upload, in microseconds (for the HUD).
pub static LAST_UPLOAD_US: AtomicU32 = AtomicU32::new(0);

const SHADER: &str = r#"
struct U {
    scale: vec2<f32>,
    offset: vec2<f32>,
    bg: vec4<f32>,
    // x: nearest sampling, y: linearize output (sRGB target), z: full range, w: BT.601
    params: vec4<f32>,
    // A circular mask, in framebuffer pixels: centre (x, y), radius (z; 0 = none), soft edge (w).
    mask: vec4<f32>,
};
@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp_linear: sampler;
@group(0) @binding(3) var samp_nearest: sampler;
@group(0) @binding(4) var<uniform> u: U;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    // Panel coordinates: (0,0) top-left, (1,1) bottom-right.
    @location(0) p: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VOut;
    out.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.p = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    // Premultiplied alpha: 1 everywhere, except at a mask's soft edge.
    var a = 1.0;
    if (u.mask.z > 0.0) {
        let d = distance(in.pos.xy, u.mask.xy);
        if (d > u.mask.z) {
            discard;
        }
        a = clamp((u.mask.z - d) / max(u.mask.w, 0.001), 0.0, 1.0);
    }
    let uv = in.p * u.scale + u.offset;
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec4<f32>(u.bg.rgb * a, a);
    }
    var y: f32;
    var c: vec2<f32>;
    if (u.params.x > 0.5) {
        y = textureSampleLevel(tex_y, samp_nearest, uv, 0.0).r;
        c = textureSampleLevel(tex_uv, samp_nearest, uv, 0.0).rg;
    } else {
        y = textureSampleLevel(tex_y, samp_linear, uv, 0.0).r;
        c = textureSampleLevel(tex_uv, samp_linear, uv, 0.0).rg;
    }
    var yy = y;
    var cb = c.x - 0.5;
    var cr = c.y - 0.5;
    if (u.params.z < 0.5) {
        yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    }
    var rgb: vec3<f32>;
    if (u.params.w > 0.5) {
        rgb = vec3<f32>(yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb);
    } else {
        rgb = vec3<f32>(yy + 1.5748 * cr, yy - 0.187324 * cb - 0.468124 * cr, yy + 1.8556 * cb);
    }
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (u.params.y > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb * a, a);
}
"#;

struct Planes {
    y: wgpu::Texture,
    uv: wgpu::Texture,
    /// One per uniform slot (the plain draw, the masked one).
    bind_groups: [wgpu::BindGroup; 2],
    width: u32,
    height: u32,
}

/// GPU resources, stored once in egui_wgpu's callback resources.
pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    linear: wgpu::Sampler,
    nearest: wgpu::Sampler,
    /// Slot 0: the video; slot 1: the masked repeat (both are prepared before either paints).
    uniforms: [wgpu::Buffer; 2],
    planes: Option<Planes>,
    /// (media generation, presented frame) currently in the textures.
    shown: Option<(u64, usize)>,
    srgb_target: bool,
    /// A frame size the card can't hold (said once in the log).
    refused: Option<(u32, u32)>,
}

impl VideoRenderer {
    pub fn install(rs: &RenderState) {
        let renderer = Self::new(&rs.device, rs.target_format);
        rs.renderer.write().callback_resources.insert(renderer);
    }

    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video nv12"),
            entries: &[
                tex(0),
                tex(1),
                samp(2),
                samp(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video nv12"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    // Opaque except at a mask's soft edge (alpha 1 replaces what is there).
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("video"),
                mag_filter: filter,
                min_filter: filter,
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            })
        };
        let uniforms = std::array::from_fn(|_| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("video uniforms"),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        Self {
            pipeline,
            layout,
            linear: sampler(wgpu::FilterMode::Linear),
            nearest: sampler(wgpu::FilterMode::Nearest),
            uniforms,
            planes: None,
            shown: None,
            srgb_target: target.is_srgb(),
            refused: None,
        }
    }

    fn ensure_planes(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if self.planes.as_ref().is_some_and(|p| p.width == width && p.height == height) {
            return;
        }
        // Larger than the card's textures (or empty): no video rather than
        // invalid textures that would fail every frame.
        let max = device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            if self.refused != Some((width, height)) {
                tracing::warn!("the video is {width}×{height}: larger than this graphics card's textures ({max} px), so it is not shown");
                self.refused = Some((width, height));
            }
            (self.planes, self.shown) = (None, None);
            return;
        }
        let texture = |label, w, h, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = texture("video y", width, height, wgpu::TextureFormat::R8Unorm);
        let uv = texture("video uv", width.div_ceil(2), height.div_ceil(2), wgpu::TextureFormat::Rg8Unorm);
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_groups = std::array::from_fn(|slot| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("video nv12"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view(&y)) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view(&uv)) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.linear) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.nearest) },
                    wgpu::BindGroupEntry { binding: 4, resource: self.uniforms[slot].as_entire_binding() },
                ],
            })
        });
        self.planes = Some(Planes { y, uv, bind_groups, width, height });
        self.shown = None;
    }

    fn upload(&mut self, queue: &wgpu::Queue, data: &[u8]) {
        let Some(p) = &self.planes else { return };
        let (w, h) = (p.width, p.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_len = (w * h) as usize;
        let copy = |texture: &wgpu::Texture, bytes: &[u8], width: u32, height: u32, bpp: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * bpp), rows_per_image: Some(height) },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        };
        copy(&p.y, &data[..y_len], w, h, 1);
        copy(&p.uv, &data[y_len..y_len + (cw * ch * 2) as usize], cw, ch, 2);
    }
}

/// One frame's draw: which frame, and how panel coordinates map to video UVs.
#[derive(Clone)]
pub struct VideoPaint {
    /// (media generation, presented frame) identifying `data`.
    pub key: (u64, usize),
    pub data: FrameData,
    pub width: u32,
    pub height: u32,
    /// uv = panel01 * scale + offset
    pub scale: [f32; 2],
    pub offset: [f32; 2],
    pub background: [f32; 4],
    pub nearest: bool,
    pub color: ColorInfo,
    /// Draw only inside this circle (centre and radius in points), with a soft
    /// edge: the video repeated over what was painted on it. At most one per frame.
    pub mask: Option<(egui::Pos2, f32)>,
}

impl VideoPaint {
    fn slot(&self) -> usize {
        usize::from(self.mask.is_some())
    }
}

impl CallbackTrait for VideoPaint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<VideoRenderer>() else { return Vec::new() };
        r.ensure_planes(device, self.width, self.height);
        if r.shown != Some(self.key) {
            let t = std::time::Instant::now();
            r.upload(queue, &self.data);
            r.shown = Some(self.key);
            LAST_UPLOAD_US.store(t.elapsed().as_micros() as u32, Ordering::Relaxed);
        }
        let params = [
            if self.nearest { 1.0 } else { 0.0 },
            if r.srgb_target { 1.0 } else { 0.0 },
            if self.color.full_range { 1.0 } else { 0.0 },
            if self.color.matrix == Matrix::Bt601 { 1.0 } else { 0.0 },
        ];
        // The mask in framebuffer pixels, with a 1.5 px soft edge.
        let ppp = screen.pixels_per_point;
        let mask = self.mask.map_or([0.0; 4], |(c, radius)| [c.x * ppp, c.y * ppp, radius * ppp, 1.5]);
        let values: [f32; 16] = [
            self.scale[0], self.scale[1], self.offset[0], self.offset[1],
            self.background[0], self.background[1], self.background[2], self.background[3],
            params[0], params[1], params[2], params[3],
            mask[0], mask[1], mask[2], mask[3],
        ];
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        queue.write_buffer(&r.uniforms[self.slot()], 0, &bytes);
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(r) = resources.get::<VideoRenderer>() else { return };
        let Some(p) = &r.planes else { return };
        pass.set_pipeline(&r.pipeline);
        pass.set_bind_group(0, &p.bind_groups[self.slot()], &[]);
        pass.draw(0..3, 0..1);
    }
}
