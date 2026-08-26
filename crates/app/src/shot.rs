//! Render a file to a PNG without opening a window.
//!
//! Two jobs. It is how the renderer gets checked without a human squinting at a screenshot —
//! "does a 360 photo actually wrap onto a sphere" is a question with a pixel answer — and it is
//! the beginning of a thumbnail path, which is the other half of belonging in a shell.
//!
//! The device here is created from scratch rather than borrowed from the window, so this runs on a
//! machine with no display at all.

use std::path::Path;

use viewer_render::{Camera, FlatView, Renderer, View};
use viewer_scene::Scene;

/// Row pitch for a buffer copy has to be a multiple of this, so the readback buffer is usually
/// wider than the image and every row has to be trimmed on the way out.
const COPY_ALIGNMENT: u32 = 256;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Options {
    pub width: u32,
    pub height: u32,
    /// Presentation time to render, for anything that moves.
    pub time_us: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            width: 1280,
            height: 720,
            time_us: 0,
        }
    }
}

pub fn render(input: &Path, output: &Path, options: &Options) -> Result<(), String> {
    let width = options.width.clamp(16, 16_384);
    let height = options.height.clamp(16, 16_384);

    let source = viewer_io::Source::open(input).map_err(|e| e.to_string())?;
    let best = source.identify().remove(0);
    let mut opened = viewer_formats::open_path(&best.format, source.bytes(), input)
        .map_err(|e| e.to_string())?;
    let scene: Scene = std::mem::take(&mut opened.scene);

    let (device, queue) = device()?;
    let renderer = Renderer::new(&device, FORMAT);
    let mut gpu = renderer.upload(&device, &queue, &scene);
    if gpu.is_empty() {
        return Err(format!(
            "{}: the file contains nothing to show",
            input.display()
        ));
    }

    // A moving file is rendered at a requested instant, which means decoding up to it. Frames are
    // pulled in order because that is the only thing every source supports; a seekable one will
    // have skipped ahead internally on the first call.
    if let Some(mut frames) = opened.frames {
        let step = scene
            .timeline
            .as_ref()
            .filter(|t| t.fps > 0.0)
            .map(|t| (1_000_000.0 / t.fps) as u64)
            .unwrap_or(33_333)
            .max(1);
        let mut pts = 0u64;
        loop {
            match frames.frame_at(pts) {
                Ok(Some(frame)) => {
                    if let Some(mesh) = &frame.mesh {
                        renderer.update_mesh(&device, &queue, &mut gpu, mesh);
                    }
                    if let Some(image) = &frame.image {
                        renderer.update_image(&device, &queue, &mut gpu, image);
                    }
                }
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            if pts >= options.time_us {
                break;
            }
            pts = (pts + step).min(options.time_us);
        }
    }

    let mut camera = Camera::default();
    camera.frame(&gpu.bounds);
    // A panorama has no outside to orbit; looking straight ahead down -Z puts the centre of the
    // image in the middle of the frame, which is the view its author composed.
    if gpu.screen().is_some_and(|s| s.is_panorama()) {
        camera.yaw = 0.0;
        camera.pitch = 0.0;
    }

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shot"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());

    let mut renderer = renderer;
    renderer.prepare(
        &queue,
        &mut gpu,
        &View {
            camera: &camera,
            viewport: (width as f32, height as f32),
            point_size: 2.0,
            volume: &viewer_render::VolumeView::default(),
            flat: &FlatView::default(),
        },
    );
    renderer.ensure_depth(&device, width, height);
    let depth = renderer
        .depth()
        .ok_or("the depth attachment could not be created")?;

    let row_bytes = width * 4;
    let padded = row_bytes.div_ceil(COPY_ALIGNMENT) * COPY_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("shot"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shot"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.055,
                        g: 0.063,
                        b: 0.075,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        renderer.draw(&mut pass, &gpu);
    }
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    // Nothing else is waiting on this thread, so a blocking poll is exactly right here.
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|_| "the readback never completed".to_string())?
        .map_err(|e| e.to_string())?;

    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((row_bytes * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&mapped[start..start + row_bytes as usize]);
    }
    drop(mapped);
    readback.unmap();

    // The target is an sRGB format, so what came back is already display-encoded and goes into
    // the file unchanged.
    image::save_buffer(output, &pixels, width, height, image::ColorType::Rgba8)
        .map_err(|e| e.to_string())
}

fn device() -> Result<(wgpu::Device, wgpu::Queue), String> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok_or("no GPU adapter is available")?;
    pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("offscreen"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: Default::default(),
        },
        None,
    ))
    .map_err(|e| e.to_string())
}
