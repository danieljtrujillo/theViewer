//! wgpu resources and draw submission.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use viewer_scene::attr::Semantic;
use viewer_scene::payload::{Indices, Payload};
use viewer_scene::{Aabb, MeshFrame, Scene, Screen};

use crate::camera::Camera;
use crate::screen::{FlatView, ScreenUniform};

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view: [[f32; 4]; 4],
    proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    viewport: [f32; 4],
    params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MeshVertex {
    pos: [f32; 3],
    nrm: [f32; 3],
    col: [f32; 4],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MaterialUniform {
    base_color: [f32; 4],
    emissive: [f32; 4],
    /// `has_texture`, `alpha_cutoff`, `unlit`, unused.
    flags: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PointVertex {
    pos: [f32; 3],
    col: [f32; 4],
}

/// 64 bytes, matching `Splat` in the shader. The padding is load-bearing: WGSL aligns `vec3` to
/// 16 bytes, so removing it silently misreads every field after the first.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuSplat {
    pos: [f32; 3],
    opacity: f32,
    scale: [f32; 3],
    _pad0: f32,
    rot: [f32; 4],
    rgb: [f32; 3],
    _pad1: f32,
}

enum Draw {
    Mesh {
        vertices: wgpu::Buffer,
        indices: Option<(wgpu::Buffer, wgpu::IndexFormat, u32)>,
        count: u32,
        /// Index into [`GpuScene::materials`].
        material: usize,
    },
    Points {
        vertices: wgpu::Buffer,
        count: u32,
    },
    Splats {
        bind: wgpu::BindGroup,
        order: wgpu::Buffer,
        centres: Vec<Vec3>,
        count: u32,
    },
    Volume {
        bind: wgpu::BindGroup,
        uniform: wgpu::Buffer,
        vertices: wgpu::Buffer,
        bounds: Aabb,
    },
    Screen {
        bind: wgpu::BindGroup,
        uniform: wgpu::Buffer,
        screen: Screen,
    },
}

/// One uploaded image, kept whole rather than reduced to a view, because a video frame that
/// changes size has to replace the texture and every bind group that named it.
struct ImageSlot {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// What a material bind group was built from, so it can be rebuilt when its texture is replaced.
#[derive(Clone, Copy)]
struct MaterialSpec {
    uniform: MaterialUniform,
    image: Option<u32>,
}

/// Everything uploaded for one file.
pub struct GpuScene {
    draws: Vec<Draw>,
    /// One bind group per material, plus a default at the end for meshes without one.
    materials: Vec<wgpu::BindGroup>,
    material_specs: Vec<MaterialSpec>,
    images: Vec<ImageSlot>,
    /// The image slot a player rewrites each frame, if there is one.
    dynamic_image: Option<u32>,
    /// The draw whose geometry a player replaces each frame, if there is one.
    dynamic_mesh: Option<usize>,
    pub bounds: Aabb,
    pub splat_count: usize,
    /// True when the scene holds a volume, so the app can show the volume controls.
    pub has_volume: bool,
    /// Source intensity range of the first volume, for reporting window and level in real units.
    pub volume_range: Option<(f32, f32)>,
    /// Camera position the splat order was last computed for.
    last_sort_eye: Option<Vec3>,
    sort_scratch: Vec<u32>,
}

impl GpuScene {
    pub fn is_empty(&self) -> bool {
        self.draws.is_empty()
    }

    /// The picture this scene is, if it is one. The app reads this to choose its view mode and to
    /// offer the projection toggle.
    pub fn screen(&self) -> Option<&Screen> {
        self.draws.iter().find_map(|d| match d {
            Draw::Screen { screen, .. } => Some(screen),
            _ => None,
        })
    }

    /// Override the projection of the screen payload: the "this 2:1 photograph is not actually a
    /// panorama" button, and its opposite.
    pub fn set_projection(&mut self, projection: viewer_scene::ScreenProjection) {
        for d in &mut self.draws {
            if let Draw::Screen { screen, .. } = d {
                screen.projection = projection;
            }
        }
    }

    pub fn set_stereo(&mut self, stereo: viewer_scene::StereoLayout) {
        for d in &mut self.draws {
            if let Draw::Screen { screen, .. } = d {
                screen.stereo = stereo;
            }
        }
    }

    pub fn has_dynamic_image(&self) -> bool {
        self.dynamic_image.is_some()
    }
}

pub struct Renderer {
    mesh_pipeline: wgpu::RenderPipeline,
    point_pipeline: wgpu::RenderPipeline,
    splat_pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    uniform_bind: wgpu::BindGroup,
    splat_layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    volume_pipeline: wgpu::RenderPipeline,
    volume_layout: wgpu::BindGroupLayout,
    volume_sampler: wgpu::Sampler,
    flat_pipeline: wgpu::RenderPipeline,
    pano_pipeline: wgpu::RenderPipeline,
    screen_layout: wgpu::BindGroupLayout,
    screen_sampler: wgpu::Sampler,
    sampler: wgpu::Sampler,
    depth: Option<(wgpu::TextureView, u32, u32)>,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        // Two modules rather than one. Bind group indices are per module, so keeping mesh and
        // splat separate lets each put its own resources at group 1 instead of forcing the mesh
        // path to declare, and therefore bind, the splat storage buffers it never reads.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mesh and point shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("mesh.wgsl").into()),
        });
        let splat_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("splat shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("splat.wgsl").into()),
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
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

        let storage = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let splat_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("splats"),
            entries: &[storage(0), storage(1)],
        });

        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("material"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("base colour"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let mesh_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mesh"),
            bind_group_layouts: &[&uniform_layout, &material_layout],
            push_constant_ranges: &[],
        });
        let splat_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("splat"),
                bind_group_layouts: &[&uniform_layout, &splat_layout],
                push_constant_ranges: &[],
            });

        let depth_write = wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::Less,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };
        // Splats are alpha-composited in sorted order, so they test against opaque geometry but do
        // not write depth. Writing would let a near splat occlude the ones blended after it.
        let depth_test_only = wgpu::DepthStencilState {
            depth_write_enabled: false,
            ..depth_write.clone()
        };

        let opaque_target = wgpu::ColorTargetState {
            format: target,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        };
        let premultiplied_target = wgpu::ColorTargetState {
            format: target,
            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        };

        let mesh_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mesh"),
            layout: Some(&mesh_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_mesh"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<MeshVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x3, 1 => Float32x3, 2 => Float32x4, 3 => Float32x2
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_mesh"),
                compilation_options: Default::default(),
                targets: &[Some(opaque_target.clone())],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(depth_write.clone()),
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        let point_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("points"),
            layout: Some(&mesh_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_point"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<PointVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_point"),
                compilation_options: Default::default(),
                targets: &[Some(opaque_target)],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(depth_write.clone()),
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        let splat_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("splats"),
            layout: Some(&splat_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &splat_shader,
                entry_point: Some("vs_splat"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &splat_shader,
                entry_point: Some("fs_splat"),
                compilation_options: Default::default(),
                targets: &[Some(premultiplied_target)],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(depth_test_only),
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        // ---- volume ----
        let volume_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("volume shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("volume.wgsl").into()),
        });
        let volume_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("volume"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let volume_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("volume"),
            // Clamping keeps the edge voxels from wrapping around and smearing across the far face.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let volume_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("volume"),
                bind_group_layouts: &[&uniform_layout, &volume_layout],
                push_constant_ranges: &[],
            });
        let volume_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("volume"),
            layout: Some(&volume_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &volume_shader,
                entry_point: Some("vs_volume"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &volume_shader,
                entry_point: Some("fs_volume"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                // The camera may end up inside the box, so both faces have to rasterize.
                cull_mode: None,
                ..Default::default()
            },
            // Depth tested but not written: the march produces one colour for a whole span, and a
            // single depth value for it would be arbitrary.
            depth_stencil: Some(wgpu::DepthStencilState {
                depth_write_enabled: false,
                ..depth_write.clone()
            }),
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        // ---- screen: a flat picture, and the same pixels as a panorama ----
        let screen_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("screen shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("screen.wgsl").into()),
        });
        // Same shape as the material layout, but its own object: the two are bound at the same
        // index by different pipelines, and sharing one would tie their lifetimes together for no
        // reason.
        let screen_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("screen"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let screen_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("screen"),
            // Repeat horizontally so a full panorama filters correctly across the seam where
            // longitude wraps; clamp vertically so the poles do not fold back on themselves.
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let screen_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("screen"),
                bind_group_layouts: &[&uniform_layout, &screen_layout],
                push_constant_ranges: &[],
            });
        let screen_pipeline = |label: &str, vs: &str, fs: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&screen_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &screen_shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &screen_shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
                    ..Default::default()
                },
                // A picture is the whole view: nothing to be behind, nothing to occlude. It is
                // still declared, and declared with the pass's format, because wgpu requires the
                // pipeline and the render pass to agree on their attachments -- a pipeline with
                // no depth state cannot be used in a pass that has one.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: false,
                    depth_compare: wgpu::CompareFunction::Always,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: Default::default(),
                multiview: None,
                cache: None,
            })
        };
        let flat_pipeline = screen_pipeline("flat picture", "vs_flat", "fs_flat");
        let pano_pipeline = screen_pipeline("panorama", "vs_pano", "fs_pano");

        Renderer {
            mesh_pipeline,
            point_pipeline,
            splat_pipeline,
            volume_pipeline,
            volume_layout,
            volume_sampler,
            flat_pipeline,
            pano_pipeline,
            screen_layout,
            screen_sampler,
            uniform_buf,
            uniform_bind,
            splat_layout,
            material_layout,
            sampler,
            depth: None,
        }
    }

    /// Allocate the depth attachment if the size changed.
    ///
    /// Split from [`Renderer::depth`] so the caller can take an immutable borrow for the whole
    /// render pass. `wgpu::TextureView` is not `Clone`, so returning one from a `&mut self` method
    /// would hold a mutable borrow across the draw calls.
    pub fn ensure_depth(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let stale = match &self.depth {
            Some((_, w, h)) => *w != width || *h != height,
            None => true,
        };
        if stale {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("depth"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            self.depth = Some((view, width, height));
        }
    }

    pub fn depth(&self) -> Option<&wgpu::TextureView> {
        self.depth.as_ref().map(|(v, _, _)| v)
    }

    /// Upload a scene. Everything is converted to `f32` here, which is a known deviation from the
    /// quantized-end-to-end posture in `docs/ARCHITECTURE.md` and the first thing to revisit once
    /// the format coverage settles.
    /// Upload one image as an sRGB texture.
    fn upload_image(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &viewer_scene::Image,
    ) -> ImageSlot {
        let width = image.width.max(1);
        let height = image.height.max(1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("base colour"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Colour textures are authored in sRGB; sampling them as linear washes everything out.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let slot = ImageSlot {
            texture,
            view,
            width,
            height,
        };
        write_image(queue, &slot, image);
        slot
    }

    fn screen_bind_group(
        &self,
        device: &wgpu::Device,
        uniform: &wgpu::Buffer,
        view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("screen"),
            layout: &self.screen_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.screen_sampler),
                },
            ],
        })
    }

    fn material_bind_group(
        &self,
        device: &wgpu::Device,
        uniform: MaterialUniform,
        view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("material"),
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("material"),
            layout: &self.material_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    pub fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue, scene: &Scene) -> GpuScene {
        let mut draws = Vec::new();
        let mut splat_count = 0usize;
        let mut has_volume = false;
        let mut volume_range: Option<(f32, f32)> = None;

        // A single white pixel stands in wherever a material has no texture, so every material
        // binds the same layout and the pipeline never needs a second variant. It is not kept
        // past this function: a bind group holds its own reference to everything it names.
        let white = self.upload_image(device, queue, &viewer_scene::Image::white());
        let images: Vec<ImageSlot> = scene
            .images
            .iter()
            .map(|i| {
                if i.is_consistent() {
                    self.upload_image(device, queue, i)
                } else {
                    self.upload_image(device, queue, &viewer_scene::Image::white())
                }
            })
            .collect();

        let mut material_specs: Vec<MaterialSpec> = scene
            .materials
            .iter()
            .map(|m| {
                let textured = m.base_color_texture.is_some();
                MaterialSpec {
                    uniform: MaterialUniform {
                        base_color: m.base_color,
                        emissive: [m.emissive[0], m.emissive[1], m.emissive[2], 0.0],
                        flags: [
                            if textured { 1.0 } else { 0.0 },
                            match m.alpha_mode {
                                viewer_scene::AlphaMode::Mask => m.alpha_cutoff,
                                _ => 0.0,
                            },
                            // Baked lighting arrives inside the texture, so textured surfaces are
                            // not shaded again.
                            if textured { 1.0 } else { 0.0 },
                            0.0,
                        ],
                    },
                    image: m.base_color_texture,
                }
            })
            .collect();

        // The default sits at the end, so `material == materials.len() - 1` means "none declared".
        let default_material = material_specs.len();
        material_specs.push(MaterialSpec {
            uniform: MaterialUniform {
                base_color: [1.0, 1.0, 1.0, 1.0],
                emissive: [0.0; 4],
                flags: [0.0, 0.0, 0.0, 0.0],
            },
            image: None,
        });

        let materials: Vec<wgpu::BindGroup> = material_specs
            .iter()
            .map(|spec| {
                let view = spec
                    .image
                    .and_then(|i| images.get(i as usize))
                    .map_or(&white.view, |slot| &slot.view);
                self.material_bind_group(device, spec.uniform, view)
            })
            .collect();

        let mut dynamic_mesh = None;
        for (node_index, node) in scene.nodes.iter().enumerate() {
            match node.payload.as_ref() {
                Some(Payload::TriMesh(m)) => {
                    let n = m.vertex_count();
                    // A mesh a player rewrites every frame has to be writable from the start.
                    // Discovering that later means rebuilding the buffer on the first frame, and
                    // wgpu rejects the write rather than quietly reallocating.
                    let dynamic = scene
                        .dynamic_meshes
                        .contains(&viewer_scene::NodeId(node_index as u32));
                    // An empty static mesh is nothing to draw. An empty *dynamic* one is a
                    // placeholder waiting for its first frame, and skipping it here is fatal:
                    // no draw is pushed, `dynamic_mesh` stays `None`, `update_mesh` returns
                    // immediately forever, and the scene reports itself empty. That is what made
                    // a Depthkit capture, whose importer seeds exactly such a placeholder, open
                    // with "the file contains nothing to show" while its frames decoded fine.
                    if n == 0 && !dynamic {
                        continue;
                    }
                    let vertex_usage = if dynamic {
                        wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST
                    } else {
                        wgpu::BufferUsages::VERTEX
                    };
                    let index_usage = if dynamic {
                        wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST
                    } else {
                        wgpu::BufferUsages::INDEX
                    };
                    let pos = read_vec3(m.attribute(&Semantic::Position), n);
                    let nrm = m
                        .attribute(&Semantic::Normal)
                        .map(|a| read_vec3(Some(a), n))
                        .unwrap_or_else(|| derive_normals(&pos, &m.indices));
                    let col = m
                        .attribute(&Semantic::Color(0))
                        .map(|a| read_vec4(Some(a), n))
                        .unwrap_or_else(|| vec![[1.0, 1.0, 1.0, 1.0]; n]);
                    let uv = m
                        .attribute(&Semantic::TexCoord(0))
                        .map(|a| read_vec2(Some(a), n))
                        .unwrap_or_else(|| vec![[0.0, 0.0]; n]);

                    let verts: Vec<MeshVertex> = (0..n)
                        .map(|i| MeshVertex {
                            pos: pos[i],
                            nrm: nrm[i],
                            col: col[i],
                            uv: uv[i],
                        })
                        .collect();
                    // wgpu refuses a zero-length buffer, and a dynamic placeholder legitimately
                    // has no vertices yet. One dummy vertex is enough to have something to grow
                    // from; `count` stays 0, so nothing is drawn from it.
                    let seed = if verts.is_empty() {
                        vec![MeshVertex {
                            pos: [0.0; 3],
                            nrm: [0.0, 1.0, 0.0],
                            col: [1.0; 4],
                            uv: [0.0; 2],
                        }]
                    } else {
                        verts
                    };
                    let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("mesh vertices"),
                        contents: bytemuck::cast_slice(&seed),
                        usage: vertex_usage,
                    });

                    let indices = match &m.indices {
                        // A dynamic mesh that arrives without indices still needs the buffer to
                        // exist, because the first decoded frame brings a topology to write into
                        // it and a buffer cannot appear later.
                        Indices::U32(v) if v.is_empty() && dynamic => Some((
                            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("mesh indices"),
                                contents: bytemuck::cast_slice(&[0u32]),
                                usage: index_usage,
                            }),
                            wgpu::IndexFormat::Uint32,
                            0,
                        )),
                        Indices::None => None,
                        Indices::U16(v) => Some((
                            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("mesh indices"),
                                contents: bytemuck::cast_slice(v),
                                usage: index_usage,
                            }),
                            wgpu::IndexFormat::Uint16,
                            v.len() as u32,
                        )),
                        Indices::U32(v) => Some((
                            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("mesh indices"),
                                contents: bytemuck::cast_slice(v),
                                usage: index_usage,
                            }),
                            wgpu::IndexFormat::Uint32,
                            v.len() as u32,
                        )),
                    };
                    if dynamic {
                        dynamic_mesh = Some(draws.len());
                    }
                    draws.push(Draw::Mesh {
                        vertices,
                        indices,
                        count: n as u32,
                        material: m
                            .material
                            .map(|i| i as usize)
                            .filter(|i| *i < default_material)
                            .unwrap_or(default_material),
                    });
                }

                Some(Payload::PointCloud(p)) => {
                    let n = p.count;
                    if n == 0 {
                        continue;
                    }
                    let pos = read_vec3(p.attribute(&Semantic::Position), n);
                    let col = p
                        .attribute(&Semantic::Color(0))
                        .map(|a| read_vec4(Some(a), n))
                        .unwrap_or_else(|| vec![[0.85, 0.86, 0.88, 1.0]; n]);
                    let verts: Vec<PointVertex> = (0..n)
                        .map(|i| PointVertex {
                            pos: pos[i],
                            col: col[i],
                        })
                        .collect();
                    let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("points"),
                        contents: bytemuck::cast_slice(&verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    draws.push(Draw::Points {
                        vertices,
                        count: n as u32,
                    });
                }

                Some(Payload::SplatCloud(s)) => {
                    let n = s.count;
                    if n == 0 {
                        continue;
                    }
                    splat_count += n;
                    let pos = read_vec3(s.attribute(&Semantic::Position), n);
                    let scale = read_vec3(s.attribute(&Semantic::SplatScale), n);
                    let rot = read_vec4(s.attribute(&Semantic::SplatRotation), n);
                    let opacity = read_scalar(s.attribute(&Semantic::SplatOpacity), n);
                    let sh0 = s.attribute(&Semantic::SplatSh {
                        degree: 0,
                        index: 0,
                    });
                    let dc = sh0.map(|a| read_vec3(Some(a), n));

                    // Degree 0 only. Higher bands need the view direction and belong in the shader
                    // once the SH buffers are uploaded.
                    const C0: f32 = 0.282_094_79;
                    let gpu: Vec<GpuSplat> = (0..n)
                        .map(|i| {
                            let rgb = match &dc {
                                Some(d) => [
                                    (0.5 + C0 * d[i][0]).clamp(0.0, 1.0),
                                    (0.5 + C0 * d[i][1]).clamp(0.0, 1.0),
                                    (0.5 + C0 * d[i][2]).clamp(0.0, 1.0),
                                ],
                                None => [0.8, 0.8, 0.8],
                            };
                            GpuSplat {
                                pos: pos[i],
                                opacity: opacity[i],
                                scale: scale[i],
                                _pad0: 0.0,
                                rot: rot[i],
                                rgb,
                                _pad1: 0.0,
                            }
                        })
                        .collect();

                    let splat_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("splat data"),
                        contents: bytemuck::cast_slice(&gpu),
                        usage: wgpu::BufferUsages::STORAGE,
                    });
                    let order: Vec<u32> = (0..n as u32).collect();
                    let order_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("splat order"),
                        contents: bytemuck::cast_slice(&order),
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    });
                    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("splats"),
                        layout: &self.splat_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: splat_buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: order_buf.as_entire_binding(),
                            },
                        ],
                    });
                    draws.push(Draw::Splats {
                        bind,
                        order: order_buf,
                        centres: pos.iter().map(|p| Vec3::from(*p)).collect(),
                        count: n as u32,
                    });
                }

                Some(Payload::VolumeGrid(v)) => {
                    if v.voxel_count() == 0 {
                        continue;
                    }
                    let (half, range) = crate::volume::to_half(v);
                    volume_range.get_or_insert(range);
                    has_volume = true;

                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("volume"),
                        size: wgpu::Extent3d {
                            width: v.dims[0].max(1),
                            height: v.dims[1].max(1),
                            depth_or_array_layers: v.dims[2].max(1),
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D3,
                        format: wgpu::TextureFormat::R16Float,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    queue.write_texture(
                        wgpu::ImageCopyTexture {
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &half,
                        wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(v.dims[0].max(1) * 2),
                            rows_per_image: Some(v.dims[1].max(1)),
                        },
                        wgpu::Extent3d {
                            width: v.dims[0].max(1),
                            height: v.dims[1].max(1),
                            depth_or_array_layers: v.dims[2].max(1),
                        },
                    );
                    let view = texture.create_view(&Default::default());

                    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("volume"),
                        size: std::mem::size_of::<crate::volume::VolumeUniform>() as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("volume"),
                        layout: &self.volume_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: uniform.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.volume_sampler),
                            },
                        ],
                    });
                    let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("volume box"),
                        contents: bytemuck::cast_slice(&crate::volume::CUBE),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    draws.push(Draw::Volume {
                        bind,
                        uniform,
                        vertices,
                        bounds: v.bounds,
                    });
                }

                Some(Payload::Screen(s)) => {
                    let view = images
                        .get(s.image as usize)
                        .map_or(&white.view, |slot| &slot.view);
                    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("screen"),
                        size: std::mem::size_of::<ScreenUniform>() as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let bind = self.screen_bind_group(device, &uniform, view);
                    draws.push(Draw::Screen {
                        bind,
                        uniform,
                        screen: s.clone(),
                    });
                }

                None => {}
            }
        }

        GpuScene {
            draws,
            materials,
            material_specs,
            images,
            dynamic_image: scene.dynamic_images.first().copied(),
            dynamic_mesh,
            bounds: scene.bounds(),
            splat_count,
            has_volume,
            volume_range,
            last_sort_eye: None,
            sort_scratch: Vec::new(),
        }
    }

    /// Replace the contents of the scene's dynamic image: the next video frame, the next
    /// texture of a volumetric capture.
    ///
    /// Same size is the fast path and by far the common one: a straight texture write, no
    /// allocation, no bind group churn. A size change means a new texture, and then every bind
    /// group that named the old one has to be rebuilt, which is why the specs are kept.
    pub fn update_image(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &mut GpuScene,
        image: &viewer_scene::Image,
    ) {
        let Some(index) = scene.dynamic_image else {
            return;
        };
        if !image.is_consistent() {
            return;
        }
        let Some(slot) = scene.images.get(index as usize) else {
            return;
        };

        if slot.width == image.width.max(1) && slot.height == image.height.max(1) {
            write_image(queue, slot, image);
            return;
        }

        scene.images[index as usize] = self.upload_image(device, queue, image);
        let view = &scene.images[index as usize].view;

        for (i, spec) in scene.material_specs.iter().enumerate() {
            if spec.image == Some(index) {
                scene.materials[i] = self.material_bind_group(device, spec.uniform, view);
            }
        }
        for draw in &mut scene.draws {
            if let Draw::Screen {
                bind,
                uniform,
                screen,
            } = draw
            {
                if screen.image == index {
                    screen.width = image.width;
                    screen.height = image.height;
                    *bind = self.screen_bind_group(device, uniform, view);
                }
            }
        }
    }

    /// Replace the geometry of the scene's dynamic mesh.
    ///
    /// Buffers are grown, never shrunk, and grown with headroom: a volumetric capture's vertex
    /// count wanders frame to frame, and reallocating on every wobble would spend more time in the
    /// allocator than in the decoder.
    pub fn update_mesh(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &mut GpuScene,
        frame: &MeshFrame,
    ) {
        let Some(index) = scene.dynamic_mesh else {
            return;
        };
        let n = frame.positions.len();
        if n == 0 {
            return;
        }
        let has_normals = frame.normals.len() == n;
        let has_uvs = frame.uvs.len() == n;
        let derived;
        let normals: &[[f32; 3]] = if has_normals {
            &frame.normals
        } else {
            derived = derive_normals(&frame.positions, &Indices::U32(frame.indices.clone()));
            &derived
        };

        let verts: Vec<MeshVertex> = (0..n)
            .map(|i| MeshVertex {
                pos: frame.positions[i],
                nrm: normals[i],
                col: [1.0, 1.0, 1.0, 1.0],
                uv: if has_uvs { frame.uvs[i] } else { [0.0, 0.0] },
            })
            .collect();

        let Some(Draw::Mesh {
            vertices,
            indices,
            count,
            ..
        }) = scene.draws.get_mut(index)
        else {
            return;
        };

        let vertex_bytes = bytemuck::cast_slice(&verts);
        if vertices.size() < vertex_bytes.len() as u64 {
            *vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("frame vertices"),
                size: grown(vertex_bytes.len() as u64),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(vertices, 0, vertex_bytes);
        *count = n as u32;

        // An empty index list means the topology did not change, which is the normal case for
        // every predicted frame in an inter-coded stream.
        if !frame.indices.is_empty() {
            let index_bytes: &[u8] = bytemuck::cast_slice(&frame.indices);
            let needs_new = match indices {
                Some((buf, _, _)) => buf.size() < index_bytes.len() as u64,
                None => true,
            };
            if needs_new {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("frame indices"),
                    size: grown(index_bytes.len() as u64),
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                *indices = Some((buf, wgpu::IndexFormat::Uint32, frame.indices.len() as u32));
            }
            if let Some((buf, format, len)) = indices {
                *format = wgpu::IndexFormat::Uint32;
                *len = frame.indices.len() as u32;
                queue.write_buffer(buf, 0, index_bytes);
            }
        }

        if !frame.bounds.is_empty() {
            scene.bounds = frame.bounds;
        }
    }

    /// Write uniforms and, when the viewpoint has moved enough, re-sort the splats.
    pub fn prepare(&self, queue: &wgpu::Queue, scene: &mut GpuScene, view: &View<'_>) {
        let View {
            camera,
            viewport,
            point_size,
            volume: volume_view,
            flat: flat_view,
        } = *view;
        let aspect = viewport.0 / viewport.1.max(1.0);
        let view = camera.view();
        let proj = camera.proj(aspect);
        let eye = camera.eye();
        let (fx, fy) = camera.focal(viewport);

        let uniforms = Uniforms {
            view: view.to_cols_array_2d(),
            proj: proj.to_cols_array_2d(),
            view_proj: (proj * view).to_cols_array_2d(),
            cam_pos: [eye.x, eye.y, eye.z, 1.0],
            viewport: [viewport.0, viewport.1, fx, fy],
            params: [point_size, camera.znear, 0.0, 0.0],
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        // Volume and screen controls change every frame the user is dragging, so they are written
        // unconditionally rather than tracked for changes.
        for draw in &scene.draws {
            match draw {
                Draw::Volume {
                    uniform, bounds, ..
                } => {
                    let v = crate::volume::VolumeUniform::new(bounds, volume_view);
                    queue.write_buffer(uniform, 0, bytemuck::bytes_of(&v));
                }
                Draw::Screen {
                    uniform, screen, ..
                } => {
                    let s = ScreenUniform::new(screen, flat_view, camera, viewport);
                    queue.write_buffer(uniform, 0, bytemuck::bytes_of(&s));
                }
                _ => {}
            }
        }

        // Re-sorting on every frame is wasted work when the camera barely moved, and the visible
        // ordering error from a small rotation is below the blend threshold.
        let moved = match scene.last_sort_eye {
            None => true,
            Some(prev) => prev.distance(eye) > camera.distance * 0.01,
        };
        if !moved {
            return;
        }
        scene.last_sort_eye = Some(eye);

        let forward = (camera.target - eye).normalize_or_zero();
        for draw in &mut scene.draws {
            if let Draw::Splats { order, centres, .. } = draw {
                sort_back_to_front(centres, eye, forward, &mut scene.sort_scratch);
                queue.write_buffer(order, 0, bytemuck::cast_slice(&scene.sort_scratch));
            }
        }
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, scene: &GpuScene) {
        pass.set_bind_group(0, &self.uniform_bind, &[]);

        for draw in &scene.draws {
            match draw {
                Draw::Mesh {
                    vertices,
                    indices,
                    count,
                    material,
                } => {
                    pass.set_pipeline(&self.mesh_pipeline);
                    // Set every draw rather than only on change: switching to the splat pipeline
                    // uses a different layout, which invalidates group 1.
                    if let Some(bind) = scene.materials.get(*material) {
                        pass.set_bind_group(1, bind, &[]);
                    }
                    pass.set_vertex_buffer(0, vertices.slice(..));
                    match indices {
                        Some((buf, format, n)) => {
                            pass.set_index_buffer(buf.slice(..), *format);
                            pass.draw_indexed(0..*n, 0, 0..1);
                        }
                        None => pass.draw(0..*count, 0..1),
                    }
                }
                Draw::Points { vertices, count } => {
                    pass.set_pipeline(&self.point_pipeline);
                    // The point shader ignores the material, but the pipeline layout declares it,
                    // so something compatible has to be bound.
                    if let Some(bind) = scene.materials.last() {
                        pass.set_bind_group(1, bind, &[]);
                    }
                    pass.set_vertex_buffer(0, vertices.slice(..));
                    pass.draw(0..6, 0..*count);
                }
                Draw::Splats { bind, count, .. } => {
                    pass.set_pipeline(&self.splat_pipeline);
                    pass.set_bind_group(1, bind, &[]);
                    pass.draw(0..6, 0..*count);
                }
                Draw::Volume { bind, vertices, .. } => {
                    pass.set_pipeline(&self.volume_pipeline);
                    pass.set_bind_group(1, bind, &[]);
                    pass.set_vertex_buffer(0, vertices.slice(..));
                    pass.draw(0..crate::volume::CUBE.len() as u32, 0..1);
                }
                Draw::Screen { bind, screen, .. } => {
                    pass.set_pipeline(if screen.is_panorama() {
                        &self.pano_pipeline
                    } else {
                        &self.flat_pipeline
                    });
                    pass.set_bind_group(1, bind, &[]);
                    // Six vertices for the rectangle, three for the panorama's single oversized
                    // triangle.
                    pass.draw(0..if screen.is_panorama() { 3 } else { 6 }, 0..1);
                }
            }
        }
    }
}

fn write_image(queue: &wgpu::Queue, slot: &ImageSlot, image: &viewer_scene::Image) {
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &slot.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(slot.width * 4),
            rows_per_image: Some(slot.height),
        },
        wgpu::Extent3d {
            width: slot.width,
            height: slot.height,
            depth_or_array_layers: 1,
        },
    );
}

/// Everything the frame's uniforms are computed from.
///
/// One struct rather than six parameters: the set grew with every render path, and a call site
/// passing five positional floats is where a viewport gets its width and height the wrong way
/// round.
pub struct View<'a> {
    pub camera: &'a Camera,
    /// Width and height, in physical pixels.
    pub viewport: (f32, f32),
    pub point_size: f32,
    pub volume: &'a crate::volume::VolumeView,
    pub flat: &'a FlatView,
}

/// Buffer size to allocate for a request of `needed` bytes.
///
/// Half again, rounded up to 4 KB. A volumetric capture's vertex count drifts every frame, and
/// allocating exactly what this frame asked for guarantees a reallocation on the next one.
fn grown(needed: u64) -> u64 {
    let target = needed + needed / 2;
    target.div_ceil(4096) * 4096
}

/// Radix sort on the quantized view depth, descending, so the farthest splat is drawn first.
///
/// Comparison sorting at these counts costs more than the render. This is O(n) with four passes
/// over a `u32` key and is the CPU stand-in until the sort moves to a compute shader.
fn sort_back_to_front(centres: &[Vec3], eye: Vec3, forward: Vec3, out: &mut Vec<u32>) {
    let n = centres.len();
    out.clear();
    out.reserve(n);
    if n == 0 {
        return;
    }

    let mut keys: Vec<u32> = Vec::with_capacity(n);
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let depths: Vec<f32> = centres
        .iter()
        .map(|c| {
            let d = (*c - eye).dot(forward);
            min = min.min(d);
            max = max.max(d);
            d
        })
        .collect();

    let span = (max - min).max(1e-6);
    for d in &depths {
        // Invert so that a larger key means nearer, then sort ascending: far splats land first.
        let t = 1.0 - ((d - min) / span).clamp(0.0, 1.0);
        keys.push((t * u32::MAX as f32) as u32);
    }

    let mut idx: Vec<u32> = (0..n as u32).collect();
    let mut tmp_idx = vec![0u32; n];
    let mut tmp_keys = vec![0u32; n];

    for shift in [0u32, 8, 16, 24] {
        let mut counts = [0usize; 256];
        for k in &keys {
            counts[((k >> shift) & 0xFF) as usize] += 1;
        }
        let mut total = 0;
        for c in counts.iter_mut() {
            let n = *c;
            *c = total;
            total += n;
        }
        for i in 0..n {
            let bucket = ((keys[i] >> shift) & 0xFF) as usize;
            let dst = counts[bucket];
            counts[bucket] += 1;
            tmp_idx[dst] = idx[i];
            tmp_keys[dst] = keys[i];
        }
        std::mem::swap(&mut idx, &mut tmp_idx);
        std::mem::swap(&mut keys, &mut tmp_keys);
    }

    // The radix passes sort keys ascending, and the key was built inverted so that a small key
    // means far away. The result is therefore already back-to-front.
    out.extend_from_slice(&idx);
}

fn read_vec3(attr: Option<&viewer_scene::Attribute>, n: usize) -> Vec<[f32; 3]> {
    let mut out = vec![[0.0f32; 3]; n];
    if let Some(a) = attr {
        let mut tmp = [0.0f32; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if a.get_f32(i, &mut tmp) {
                *slot = [tmp[0], tmp[1], tmp[2]];
            }
        }
    }
    out
}

fn read_vec2(attr: Option<&viewer_scene::Attribute>, n: usize) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0f32; 2]; n];
    if let Some(a) = attr {
        let mut tmp = [0.0f32; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if a.get_f32(i, &mut tmp) {
                *slot = [tmp[0], tmp[1]];
            }
        }
    }
    out
}

fn read_vec4(attr: Option<&viewer_scene::Attribute>, n: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[0.0, 0.0, 0.0, 1.0]; n];
    if let Some(a) = attr {
        let mut tmp = [0.0f32; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if a.get_f32(i, &mut tmp) {
                *slot = tmp;
                if a.components < 4 {
                    slot[3] = 1.0;
                }
            }
        }
    }
    out
}

fn read_scalar(attr: Option<&viewer_scene::Attribute>, n: usize) -> Vec<f32> {
    let mut out = vec![1.0f32; n];
    if let Some(a) = attr {
        let mut tmp = [0.0f32; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if a.get_f32(i, &mut tmp) {
                *slot = tmp[0];
            }
        }
    }
    out
}

/// Area-weighted vertex normals. STL and most scan output arrive without any.
fn derive_normals(pos: &[[f32; 3]], indices: &Indices) -> Vec<[f32; 3]> {
    let mut out = vec![Vec3::ZERO; pos.len()];
    let mut add = |a: usize, b: usize, c: usize| {
        if a >= pos.len() || b >= pos.len() || c >= pos.len() {
            return;
        }
        let (va, vb, vc) = (Vec3::from(pos[a]), Vec3::from(pos[b]), Vec3::from(pos[c]));
        // Not normalized, so larger triangles contribute proportionally.
        let n = (vb - va).cross(vc - va);
        out[a] += n;
        out[b] += n;
        out[c] += n;
    };

    match indices {
        Indices::None => {
            for t in (0..pos.len().saturating_sub(2)).step_by(3) {
                add(t, t + 1, t + 2);
            }
        }
        Indices::U16(v) => {
            for t in v.chunks_exact(3) {
                add(t[0] as usize, t[1] as usize, t[2] as usize);
            }
        }
        Indices::U32(v) => {
            for t in v.chunks_exact(3) {
                add(t[0] as usize, t[1] as usize, t[2] as usize);
            }
        }
    }

    out.iter()
        .map(|n| {
            let v = n.normalize_or_zero();
            [v.x, v.y, v.z]
        })
        .collect()
}

/// Convert a bounding box to a matrix that centres and unit-scales it. Used for framing.
pub fn fit_matrix(bounds: &Aabb) -> Mat4 {
    if bounds.is_empty() {
        return Mat4::IDENTITY;
    }
    let c = bounds.center();
    Mat4::from_translation(-Vec3::from(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_splat_matches_the_shader_struct_size() {
        // WGSL aligns vec3 to 16 bytes. Any drift here misreads every field after the first.
        assert_eq!(std::mem::size_of::<GpuSplat>(), 64);
        assert_eq!(std::mem::size_of::<Uniforms>(), 64 * 3 + 16 * 3);
    }

    #[test]
    fn sort_puts_the_farthest_splat_first() {
        let eye = Vec3::new(0.0, 0.0, 10.0);
        let forward = Vec3::new(0.0, 0.0, -1.0);
        let centres = vec![
            Vec3::new(0.0, 0.0, 0.0),   // 10 away
            Vec3::new(0.0, 0.0, 9.0),   // 1 away, nearest
            Vec3::new(0.0, 0.0, -10.0), // 20 away, farthest
        ];
        let mut out = Vec::new();
        sort_back_to_front(&centres, eye, forward, &mut out);
        assert_eq!(out, vec![2, 0, 1]);
    }

    #[test]
    fn sort_handles_an_empty_cloud() {
        let mut out = vec![9u32];
        sort_back_to_front(&[], Vec3::ZERO, Vec3::Z, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn sort_handles_coincident_splats_without_dropping_any() {
        let centres = vec![Vec3::ONE; 1000];
        let mut out = Vec::new();
        sort_back_to_front(&centres, Vec3::ZERO, Vec3::Z, &mut out);
        assert_eq!(out.len(), 1000);
        let mut seen = out.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 1000, "every index must appear exactly once");
    }

    #[test]
    fn sort_is_a_permutation_for_a_large_random_cloud() {
        // Deterministic pseudo-random positions; no rand dependency.
        let mut state = 0x2545_F491u32;
        let centres: Vec<Vec3> = (0..10_000)
            .map(|_| {
                let mut next = || {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (state as f32 / u32::MAX as f32) * 200.0 - 100.0
                };
                Vec3::new(next(), next(), next())
            })
            .collect();
        let mut out = Vec::new();
        sort_back_to_front(&centres, Vec3::new(0.0, 0.0, 300.0), -Vec3::Z, &mut out);
        assert_eq!(out.len(), centres.len());
        let mut seen = out.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), centres.len());

        // Depth must be non-increasing across the output.
        let eye = Vec3::new(0.0, 0.0, 300.0);
        let mut prev = f32::INFINITY;
        for &i in &out {
            let d = (centres[i as usize] - eye).dot(-Vec3::Z);
            assert!(d <= prev + 0.02, "order broke at {d} after {prev}");
            prev = d;
        }
    }

    #[test]
    fn derived_normals_point_consistently_for_a_simple_triangle() {
        let pos = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let n = derive_normals(&pos, &Indices::U16(vec![0, 1, 2]));
        for v in n {
            assert!((v[2].abs() - 1.0).abs() < 1e-5, "expected +/-Z, got {v:?}");
        }
    }

    #[test]
    fn derived_normals_survive_degenerate_triangles() {
        let pos = vec![[0.0; 3], [0.0; 3], [0.0; 3]];
        let n = derive_normals(&pos, &Indices::U16(vec![0, 1, 2]));
        for v in n {
            assert!(v.iter().all(|c| c.is_finite()));
        }
    }
}
