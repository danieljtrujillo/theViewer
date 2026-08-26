//! Window, viewport and controls.
//!
//! One window presents five kinds of thing: geometry, splats, volumes, pictures and moving
//! pictures, and the only way that stays coherent is if the *presentation* changes with the
//! content while the controls stay in the same places. So there is one toolbar, one status bar,
//! and one viewport whose gestures are re-bound per [`Mode`]: drag orbits a model, pans a
//! photograph, and looks around a panorama, and in all three cases drag is the thing you reach
//! for first.
//!
//! The playback bar is deliberately spare: a play button, a scrub line, one timecode. Anything
//! that plays gets the same three, whether it is an MP4, an animated GIF or a volumetric capture.

use std::path::{Path, PathBuf};

use eframe::egui;
use eframe::wgpu;

use viewer_io::{Confidence, Source};
use viewer_render::{Camera, FlatView, GpuScene, Renderer};
use viewer_scene::{Metadata, Scene, ScreenProjection, StereoLayout, Summary};

use crate::player::{timecode, Player};
use crate::shell;

/// The offscreen viewport is rendered at this format, so the pipelines are built for it rather
/// than for the swapchain.
const VIEWPORT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Height of the playback strip, in points.
const BAR_HEIGHT: f32 = 34.0;

pub fn run(initial: Option<PathBuf>) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Logical points, not pixels. A 150% display makes a 1280x720 physical panel report a
            // 1280x720 logical desktop at 1920x1080, so anything taller than about 660 pushes the
            // status bar off the bottom of the screen on a common laptop setup.
            .with_inner_size([1040.0, 640.0])
            .with_min_inner_size([420.0, 300.0])
            .with_title("3D Viewer"),
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 0,
        ..Default::default()
    };
    eframe::run_native(
        "3D Viewer",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, initial)))),
    )
}

/// How the viewport is presented, and therefore what the mouse does in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Geometry, splats, volumes. Drag orbits.
    Orbit,
    /// A picture, fitted to the window. Drag pans.
    Flat,
    /// A picture wrapped around the viewer. Drag looks around.
    Panorama,
}

struct Loaded {
    name: String,
    bytes: u64,
    format: String,
    confidence: Confidence,
    summary: Summary,
    meta: Metadata,
    gpu: GpuScene,
    player: Option<Player>,
}

impl Loaded {
    fn mode(&self) -> Mode {
        match self.gpu.screen() {
            None => Mode::Orbit,
            Some(s) if s.is_panorama() => Mode::Panorama,
            Some(_) => Mode::Flat,
        }
    }
}

struct Offscreen {
    /// Held so the texture outlives the view and the egui binding that samples it.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    id: egui::TextureId,
    width: u32,
    height: u32,
}

struct App {
    renderer: Option<Renderer>,
    camera: Camera,
    flat: FlatView,
    loaded: Option<Loaded>,
    offscreen: Option<Offscreen>,
    error: Option<String>,
    point_size: f32,
    volume_view: viewer_render::VolumeView,
    pending: Option<PathBuf>,
    /// Opacity of the playback strip, eased so it does not blink in and out under the cursor.
    bar_alpha: f32,
    /// Whether the Open with question is on screen.
    asking_shell: bool,
    shell_result: Option<String>,
    show_details: bool,
    /// Whether the camera has been framed on real geometry rather than on a placeholder.
    framed_dynamic: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let renderer = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| Renderer::new(&rs.device, VIEWPORT_FORMAT));
        App {
            renderer,
            camera: Camera::default(),
            flat: FlatView::default(),
            loaded: None,
            offscreen: None,
            error: None,
            point_size: 2.0,
            volume_view: viewer_render::VolumeView::default(),
            pending: initial,
            bar_alpha: 1.0,
            asking_shell: shell::should_prompt(),
            shell_result: None,
            show_details: false,
            framed_dynamic: false,
        }
    }

    fn open(&mut self, path: &Path, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.error = None;
        // Dropping the previous player first stops its decode thread before the next one starts,
        // so two files never hold decoders open at once.
        self.loaded = None;

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();

        let source = match Source::open(path) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("{name}: {e}"));
                return;
            }
        };

        let candidates = source.identify();
        let best = &candidates[0];

        let opened = match viewer_formats::open_path(&best.format, source.bytes(), path) {
            Ok(o) => o,
            Err(e) => {
                self.error = Some(format!("{name}: {e}"));
                return;
            }
        };
        let scene: Scene = opened.scene;

        let Some(renderer) = self.renderer.as_ref() else {
            self.error = Some("The GPU device is unavailable.".into());
            return;
        };

        let gpu = renderer.upload(device, queue, &scene);
        if gpu.is_empty() {
            self.error = Some(format!("{name}: the file contains nothing to show."));
            return;
        }

        let player = opened.frames.map(|source| {
            let timeline = scene.timeline.clone().unwrap_or_else(|| source.timeline());
            Player::new(source, timeline)
        });

        self.camera.frame(&gpu.bounds);
        self.flat.reset();
        self.bar_alpha = 1.0;
        self.framed_dynamic = false;
        self.loaded = Some(Loaded {
            name,
            bytes: source.len(),
            format: best.format.label().to_string(),
            confidence: best.confidence,
            summary: scene.summary(),
            meta: scene.meta.clone(),
            gpu,
            player,
        });
    }

    fn pick(&mut self) {
        // The file dialog wants bare extensions; the shell registration wants dotted ones, and
        // that list is the single place they are enumerated.
        let extensions: Vec<&str> = shell::EXTENSIONS
            .iter()
            .map(|e| e.trim_start_matches('.'))
            .collect();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Everything the viewer opens", &extensions)
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.pending = Some(path);
        }
    }

    fn ensure_offscreen(
        &mut self,
        rs: &eframe::egui_wgpu::RenderState,
        width: u32,
        height: u32,
    ) -> egui::TextureId {
        let width = width.max(1);
        let height = height.max(1);
        let stale = match &self.offscreen {
            Some(o) => o.width != width || o.height != height,
            None => true,
        };
        if stale {
            let texture = rs.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("viewport"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: VIEWPORT_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let mut egui_renderer = rs.renderer.write();
            let id = match self.offscreen.take() {
                Some(old) => {
                    egui_renderer.update_egui_texture_from_wgpu_texture(
                        &rs.device,
                        &view,
                        wgpu::FilterMode::Linear,
                        old.id,
                    );
                    old.id
                }
                None => egui_renderer.register_native_texture(
                    &rs.device,
                    &view,
                    wgpu::FilterMode::Linear,
                ),
            };
            self.offscreen = Some(Offscreen {
                texture,
                view,
                id,
                width,
                height,
            });
        }
        self.offscreen.as_ref().unwrap().id
    }

    /// Volume controls. Shown only when a volume is loaded.
    ///
    /// Window and level are reported in the source intensity units, which for CT are Hounsfield,
    /// because the normalized 0 to 1 form the shader uses means nothing to anyone reading a scan.
    fn volume_panel(&mut self, ctx: &egui::Context) {
        use viewer_render::{Axis, Projection, VolumeMode};

        let range = self
            .loaded
            .as_ref()
            .and_then(|l| l.gpu.volume_range)
            .unwrap_or((0.0, 1.0));
        let span = (range.1 - range.0).max(1e-6);
        let v = &mut self.volume_view;

        egui::SidePanel::right("volume")
            .exact_width(216.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    let slice = v.mode == VolumeMode::Slice;
                    if ui.selectable_label(slice, "Slice").clicked() {
                        v.mode = VolumeMode::Slice;
                    }
                    if ui.selectable_label(!slice, "Volume").clicked() {
                        v.mode = VolumeMode::Raycast;
                    }
                });
                ui.add_space(6.0);

                if v.mode == VolumeMode::Slice {
                    ui.horizontal(|ui| {
                        for axis in [Axis::Axial, Axis::Coronal, Axis::Sagittal] {
                            if ui
                                .selectable_label(v.axis == axis, &axis.label()[..3])
                                .on_hover_text(axis.label())
                                .clicked()
                            {
                                v.axis = axis;
                            }
                        }
                    });
                    ui.add(egui::Slider::new(&mut v.slice, 0.0..=1.0).text("Slice"));
                } else {
                    let mip = v.projection == Projection::MaximumIntensity;
                    let mut on = mip;
                    if ui
                        .checkbox(&mut on, "MIP")
                        .on_hover_text("Maximum intensity projection.")
                        .changed()
                    {
                        v.projection = if on {
                            Projection::MaximumIntensity
                        } else {
                            Projection::Composite
                        };
                    }
                    if !on {
                        ui.add(egui::Slider::new(&mut v.threshold, 0.0..=1.0).text("Threshold"));
                        ui.add(egui::Slider::new(&mut v.opacity, 0.005..=0.4).text("Opacity"));
                    }
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                // Presented in source units, converted back to normalized on the way in.
                let mut level_units = range.0 + v.level * span;
                let mut window_units = v.window * span;
                if ui
                    .add(
                        egui::Slider::new(&mut window_units, 1.0..=span as f64 as f32)
                            .text("Window"),
                    )
                    .changed()
                {
                    v.window = (window_units / span).clamp(0.0001, 4.0);
                }
                if ui
                    .add(egui::Slider::new(&mut level_units, range.0..=range.1).text("Level"))
                    .changed()
                {
                    v.level = (level_units - range.0) / span;
                }

                ui.add_space(8.0);
                if ui.button("Reset").clicked() {
                    let keep = (v.mode, v.axis, v.projection);
                    *v = viewer_render::VolumeView::default();
                    v.mode = keep.0;
                    v.axis = keep.1;
                    v.projection = keep.2;
                }
            });
    }

    fn render_viewport(&mut self, rs: &eframe::egui_wgpu::RenderState, width: u32, height: u32) {
        let (Some(renderer), Some(loaded)) = (self.renderer.as_mut(), self.loaded.as_mut()) else {
            return;
        };
        let Some(offscreen) = self.offscreen.as_ref() else {
            return;
        };

        renderer.prepare(
            &rs.queue,
            &mut loaded.gpu,
            &viewer_render::View {
                camera: &self.camera,
                viewport: (width as f32, height as f32),
                point_size: self.point_size,
                volume: &self.volume_view,
                flat: &self.flat,
            },
        );

        renderer.ensure_depth(&rs.device, width, height);
        let Some(depth) = renderer.depth() else {
            return;
        };
        let mut encoder = rs
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewport"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewport"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &offscreen.view,
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
            renderer.draw(&mut pass, &loaded.gpu);
        }
        rs.queue.submit(Some(encoder.finish()));
    }

    /// Pull whatever the decoder produced and hand it to the GPU.
    fn advance_playback(&mut self, dt: f32, rs: &eframe::egui_wgpu::RenderState) {
        let (Some(renderer), Some(loaded)) = (self.renderer.as_ref(), self.loaded.as_mut()) else {
            return;
        };
        let Some(player) = loaded.player.as_mut() else {
            return;
        };
        let Some(frame) = player.advance(dt) else {
            return;
        };
        if let Some(mesh) = &frame.mesh {
            renderer.update_mesh(&rs.device, &rs.queue, &mut loaded.gpu, mesh);
            // A capture's declared bounds before any frame arrives are a guess. The whole camera
            // frustum, for RGBD. Framing once on the first real geometry puts the subject in the
            // window instead of leaving it a speck inside its own view volume.
            if !self.framed_dynamic && !mesh.bounds.is_empty() {
                self.framed_dynamic = true;
                self.camera.frame(&loaded.gpu.bounds);
            }
        }
        if let Some(image) = &frame.image {
            renderer.update_image(&rs.device, &rs.queue, &mut loaded.gpu, image);
        }
    }

    /// The playback strip: play, scrub, timecode. Nothing else.
    ///
    /// Drawn into the viewport rather than docked below it, so that a video fills the window and
    /// the controls sit over the picture the way they do in every other player. It fades out when
    /// the pointer leaves and never while paused, because a paused frame is one someone is
    /// looking at and the controls are what they are about to reach for.
    fn playback_bar(&mut self, ui: &mut egui::Ui, viewport: egui::Rect, pointer_inside: bool) {
        let Some(player) = self
            .loaded
            .as_mut()
            .and_then(|l| l.player.as_mut())
            .filter(|p| p.timeline.frame_count > 1)
        else {
            return;
        };

        let target = if pointer_inside || !player.playing {
            1.0
        } else {
            0.0
        };
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        // Roughly a third of a second either way. Fast enough to feel responsive, slow enough
        // that crossing the edge of the window does not flash the bar.
        let step = dt / 0.3;
        self.bar_alpha += (target - self.bar_alpha).clamp(-step, step);
        if self.bar_alpha <= 0.002 {
            return;
        }
        let alpha = self.bar_alpha;
        let fade = |c: egui::Color32| {
            egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * alpha) as u8)
        };

        let rect = egui::Rect::from_min_max(
            egui::pos2(viewport.left(), viewport.bottom() - BAR_HEIGHT),
            viewport.max,
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(
            rect,
            0.0,
            fade(egui::Color32::from_rgba_unmultiplied(10, 12, 15, 190)),
        );

        let ink = fade(egui::Color32::from_rgb(228, 232, 238));
        let dim = fade(egui::Color32::from_rgba_unmultiplied(228, 232, 238, 90));

        // --- play / pause ---
        let button = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 21.0, rect.center().y),
            egui::vec2(26.0, 26.0),
        );
        let button_response = ui.interact(button, ui.id().with("play"), egui::Sense::click());
        draw_transport(&painter, button.shrink(8.0), ink, player.playing);
        if button_response.clicked() {
            player.toggle();
        }

        // --- timecode, laid out from the right so the scrub line gets the rest ---
        let label = format!(
            "{} / {}",
            timecode(player.clock_us),
            timecode(player.duration_us())
        );
        let font = egui::FontId::proportional(11.0);
        let galley = painter.layout_no_wrap(label, font, ink);
        let text_width = galley.size().x;
        painter.galley(
            egui::pos2(
                rect.right() - 12.0 - text_width,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            ink,
        );

        // --- scrub ---
        let track = egui::Rect::from_min_max(
            egui::pos2(button.right() + 8.0, rect.center().y - 7.0),
            egui::pos2(rect.right() - text_width - 22.0, rect.center().y + 7.0),
        );
        if track.width() < 24.0 {
            return;
        }
        let scrub = ui.interact(track, ui.id().with("scrub"), egui::Sense::click_and_drag());
        let line = egui::Rect::from_min_max(
            egui::pos2(track.left(), track.center().y - 1.5),
            egui::pos2(track.right(), track.center().y + 1.5),
        );
        painter.rect_filled(line, 1.5, dim);
        let t = player.progress();
        let played = egui::Rect::from_min_max(
            line.min,
            egui::pos2(line.left() + line.width() * t, line.max.y),
        );
        painter.rect_filled(played, 1.5, ink);
        if scrub.hovered() || scrub.dragged() {
            painter.circle_filled(egui::pos2(played.right(), line.center().y), 4.5, ink);
        }

        if scrub.dragged() || scrub.clicked() {
            if let Some(pos) = scrub.interact_pointer_pos() {
                player.seek_fraction((pos.x - track.left()) / track.width().max(1.0));
            }
        }

        if let Some(message) = player.error.clone() {
            painter.text(
                egui::pos2(track.left(), rect.top() - 6.0),
                egui::Align2::LEFT_BOTTOM,
                message,
                egui::FontId::proportional(11.0),
                fade(egui::Color32::from_rgb(224, 120, 130)),
            );
        }
    }

    /// The one-time question about the Open with menu.
    fn shell_prompt(&mut self, ctx: &egui::Context) {
        if !self.asking_shell {
            return;
        }
        let mut open = true;
        egui::Window::new("Add to the Open with menu?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_max_width(430.0);
                ui.add_space(4.0);
                ui.label(
                    "3D Viewer can add itself to the Windows \"Open with\" menu for the file \
                     types it reads.",
                );
                ui.add_space(8.0);
                ui.weak(
                    "This adds an entry for the current user only. It does not change which \
                     application opens these files by default, and it can be undone from the \
                     toolbar menu at any time.",
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        self.shell_result = Some(match std::env::current_exe() {
                            Ok(exe) => match shell::register(&exe) {
                                Ok(()) => {
                                    shell::record_consent(shell::Consent::Granted);
                                    "Added to the Open with menu.".to_string()
                                }
                                Err(e) => e,
                            },
                            Err(e) => e.to_string(),
                        });
                        self.asking_shell = false;
                    }
                    if ui.button("Not now").clicked() {
                        shell::record_consent(shell::Consent::Deferred);
                        self.asking_shell = false;
                    }
                    if ui.button("Never ask again").clicked() {
                        shell::record_consent(shell::Consent::Refused);
                        self.asking_shell = false;
                    }
                });
                ui.add_space(4.0);
            });
        // Dismissing with the window's own close button is a deferral, not a refusal.
        if !open {
            shell::record_consent(shell::Consent::Deferred);
            self.asking_shell = false;
        }
    }

    /// Everything the file said about itself. Hidden by default; the status bar carries the
    /// numbers that matter at a glance.
    fn details_panel(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.as_ref() else {
            self.show_details = false;
            return;
        };
        if loaded.meta.is_empty() {
            return;
        }
        let mut open = self.show_details;
        egui::Window::new("Details")
            .open(&mut open)
            .resizable(false)
            .default_pos(egui::pos2(60.0, 70.0))
            .show(ctx, |ui| {
                egui::Grid::new("meta").num_columns(2).show(ui, |ui| {
                    for (key, value) in &loaded.meta {
                        ui.weak(key);
                        ui.label(value);
                        ui.end_row();
                    }
                });
            });
        self.show_details = open;
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                if icon_button(ui, true, "Open   Ctrl+O", icon::folder) {
                    self.pick();
                }
                let has = self.loaded.is_some();
                let mode = self.loaded.as_ref().map(Loaded::mode);
                if icon_button(ui, has, self.fit_tooltip(mode), icon::fit) {
                    self.fit();
                }

                if let Some(l) = &self.loaded {
                    ui.separator();
                    ui.label(&l.name);
                    ui.separator();
                    ui.weak(&l.format);
                    // Only qualify a verdict that is not backed by a magic number or by structure.
                    match l.confidence {
                        Confidence::Heuristic => {
                            ui.weak("heuristic").on_hover_text(
                                "The format was inferred from content patterns rather than a magic number.",
                            );
                        }
                        Confidence::ExtensionOnly => {
                            ui.weak("extension").on_hover_text(
                                "The format was inferred from the file name alone.",
                            );
                        }
                        Confidence::Structural | Confidence::Magic => {}
                    }
                }

                // Picture controls: the projection guess is one click from being corrected, and
                // a stereo pair is one click from showing the other eye.
                if let Some(screen) = self.loaded.as_ref().and_then(|l| l.gpu.screen()) {
                    let panorama = screen.is_panorama();
                    let stereo = screen.stereo;
                    ui.separator();
                    if ui
                        .selectable_label(panorama, "360")
                        .on_hover_text(if panorama {
                            "Showing this as a panorama. Click to show the flat image instead."
                        } else {
                            "Show this as a 360 panorama."
                        })
                        .clicked()
                    {
                        self.set_panorama(!panorama);
                    }
                    if stereo != StereoLayout::Mono {
                        let mut right = self.flat.right_eye;
                        if ui
                            .selectable_label(right, if right { "R" } else { "L" })
                            .on_hover_text("Stereo pair: switch eyes.")
                            .clicked()
                        {
                            right = !right;
                            self.flat.right_eye = right;
                        }
                    }
                }

                if self.loaded.as_ref().is_some_and(|l| l.summary.points > 0) {
                    ui.separator();
                    ui.add(
                        egui::Slider::new(&mut self.point_size, 1.0..=12.0)
                            .text("px")
                            .fixed_decimals(0),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(4.0);
                    ui.menu_button("\u{22ee}", |ui| {
                        let registered = shell::is_registered();
                        if registered {
                            if ui.button("Remove from the Open with menu").clicked() {
                                self.shell_result = Some(match shell::unregister() {
                                    Ok(()) => {
                                        shell::record_consent(shell::Consent::Deferred);
                                        "Removed from the Open with menu.".into()
                                    }
                                    Err(e) => e,
                                });
                                ui.close_menu();
                            }
                        } else if ui.button("Add to the Open with menu").clicked() {
                            self.asking_shell = true;
                            ui.close_menu();
                        }
                        let has_details = self
                            .loaded
                            .as_ref()
                            .is_some_and(|l| !l.meta.is_empty());
                        if ui
                            .add_enabled(has_details, egui::Button::new("Details"))
                            .clicked()
                        {
                            self.show_details = !self.show_details;
                            ui.close_menu();
                        }
                    });
                    if let Some(message) = &self.shell_result {
                        ui.weak(message);
                    }
                });
            });
            ui.add_space(2.0);
        });
    }

    fn fit_tooltip(&self, mode: Option<Mode>) -> &'static str {
        match mode {
            Some(Mode::Flat) => "Fit to window   F",
            Some(Mode::Panorama) => "Reset the view   F",
            _ => "Fit   F",
        }
    }

    fn fit(&mut self) {
        let Some(l) = &self.loaded else {
            return;
        };
        match l.mode() {
            Mode::Flat => self.flat.reset(),
            Mode::Panorama => {
                self.camera = Camera::default();
                self.camera.yaw = 0.0;
                self.camera.pitch = 0.0;
            }
            Mode::Orbit => self.camera.frame(&l.gpu.bounds),
        }
    }

    fn set_panorama(&mut self, on: bool) {
        let Some(l) = self.loaded.as_mut() else {
            return;
        };
        l.gpu.set_projection(if on {
            // A panorama that arrived without metadata is treated as a full turn; VR180 files
            // reach this path with their own arc already set by the importer.
            ScreenProjection::Equirect {
                horizontal_fov: 360.0,
            }
        } else {
            ScreenProjection::Flat
        });
        self.flat.reset();
        self.camera = Camera::default();
        self.camera.yaw = 0.0;
        self.camera.pitch = 0.0;
    }

    /// Viewport gestures, re-bound per mode.
    fn handle_viewport_input(
        &mut self,
        ctx: &egui::Context,
        response: &egui::Response,
        size: egui::Vec2,
        mode: Mode,
    ) {
        let drag = response.drag_delta();

        match mode {
            Mode::Orbit => {
                if response.dragged_by(egui::PointerButton::Primary) {
                    self.camera.orbit(drag.x * 0.008, drag.y * 0.008);
                }
                if response.dragged_by(egui::PointerButton::Secondary)
                    || response.dragged_by(egui::PointerButton::Middle)
                {
                    self.camera
                        .pan(drag.x / size.x.max(1.0), drag.y / size.y.max(1.0));
                }
            }
            Mode::Panorama => {
                if response.dragged() {
                    // Negated relative to orbit: inside a sphere the content should follow the
                    // cursor, the way dragging a map does. Orbiting a model is the opposite
                    // gesture -- there the object follows and the camera does not.
                    self.camera.orbit(-drag.x * 0.005, -drag.y * 0.005);
                }
            }
            Mode::Flat => {
                if response.dragged() {
                    self.flat.pan(
                        2.0 * drag.x / size.x.max(1.0),
                        -2.0 * drag.y / size.y.max(1.0),
                    );
                }
                if response.double_clicked() {
                    // Fit and one-to-one are the two zooms anyone actually wants; a double click
                    // toggles between them.
                    let one_to_one = self.one_to_one_zoom(size);
                    self.flat.zoom = if (self.flat.zoom - 1.0).abs() < 1e-3 {
                        one_to_one
                    } else {
                        1.0
                    };
                    if self.flat.zoom <= 1.0 {
                        self.flat.offset = [0.0, 0.0];
                    }
                }
            }
        }

        if !response.hovered() {
            return;
        }
        let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() <= 0.0 {
            return;
        }
        match mode {
            Mode::Flat => self.flat.zoom_by((scroll * 0.004).exp()),
            Mode::Panorama => {
                // Zoom is field of view, not distance: there is no distance to change.
                let fov = self.camera.fov_y.to_degrees() - scroll * 0.09;
                self.camera.fov_y = fov.clamp(20.0, 110.0).to_radians();
            }
            Mode::Orbit => {
                // On a slice view the wheel steps through slices, which is the console
                // convention. Everywhere else it zooms.
                let slicing = self.loaded.as_ref().is_some_and(|l| l.gpu.has_volume)
                    && self.volume_view.mode == viewer_render::VolumeMode::Slice;
                if slicing {
                    self.volume_view.slice =
                        (self.volume_view.slice - scroll * 0.002).clamp(0.0, 1.0);
                } else {
                    self.camera.zoom(scroll * 0.02);
                }
            }
        }
    }

    fn one_to_one_zoom(&self, size: egui::Vec2) -> f32 {
        let Some(screen) = self.loaded.as_ref().and_then(|l| l.gpu.screen()) else {
            return 1.0;
        };
        viewer_render::zoom_for_one_to_one((screen.width, screen.height), (size.x, size.y))
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.command) {
            self.pick();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.fit();
        }

        let Some(player) = self.loaded.as_mut().and_then(|l| l.player.as_mut()) else {
            return;
        };
        if player.timeline.frame_count <= 1 {
            return;
        }
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Space) {
                player.toggle();
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                player.step(-1);
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                player.step(1);
            }
            if i.key_pressed(egui::Key::Home) {
                player.seek_to(0);
            }
            if i.key_pressed(egui::Key::End) {
                player.seek_to(player.duration_us());
            }
            if i.key_pressed(egui::Key::L) {
                player.looping = !player.looping;
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let render_state = frame.wgpu_render_state().cloned();

        // Files arriving by drop or from the command line.
        ctx.input(|i| {
            if let Some(f) = i.raw.dropped_files.first() {
                if let Some(p) = &f.path {
                    self.pending = Some(p.clone());
                }
            }
        });
        self.handle_keys(ctx);
        if let (Some(path), Some(rs)) = (self.pending.take(), render_state.as_ref()) {
            self.open(&path, &rs.device, &rs.queue);
        }

        if let Some(rs) = render_state.as_ref() {
            let dt = ctx.input(|i| i.stable_dt);
            self.advance_playback(dt, rs);
        }

        self.toolbar(ctx);

        // A fixed height keeps the row from growing: a nested right-aligned layout inside a
        // horizontal claims the panel's whole available height, which silently inflates the bar.
        egui::TopBottomPanel::bottom("status")
            .exact_height(26.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| match (&self.loaded, &self.error) {
                    (_, Some(e)) => {
                        ui.colored_label(egui::Color32::from_rgb(184, 60, 78), e);
                    }
                    (Some(l), None) => {
                        stats(ui, &l.summary, l.bytes);
                        if let Some(player) = &l.player {
                            if player.timeline.frame_count > 1 {
                                ui.separator();
                                ui.weak("Frame");
                                ui.label(format!(
                                    "{} / {}",
                                    player.current_frame() + 1,
                                    player.timeline.frame_count
                                ));
                            }
                        }
                        let dt = ctx.input(|i| i.stable_dt);
                        if dt > 0.0 {
                            ui.separator();
                            ui.weak(format!("{:.0} fps", 1.0 / dt));
                        }
                    }
                    (None, None) => {}
                });
            });

        if self.loaded.as_ref().is_some_and(|l| l.gpu.has_volume) {
            self.volume_panel(ctx);
        }

        // Matches the viewport clear colour after the sRGB encode, so the background does not
        // shift when a file loads.
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(66, 70, 76)))
            .show(ctx, |ui| {
                let size = ui.available_size();
                if self.loaded.is_none() {
                    empty_state(ui, size);
                    return;
                }
                let Some(rs) = render_state.as_ref() else {
                    return;
                };

                let ppp = ctx.pixels_per_point();
                let w = (size.x * ppp) as u32;
                let h = (size.y * ppp) as u32;
                let id = self.ensure_offscreen(rs, w, h);

                let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());

                let mode = self
                    .loaded
                    .as_ref()
                    .map(Loaded::mode)
                    .unwrap_or(Mode::Orbit);
                self.handle_viewport_input(ctx, &response, size, mode);

                self.render_viewport(rs, w, h);
                ui.painter().image(
                    id,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                let pointer_inside = ctx
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(|p| rect.contains(p));
                self.playback_bar(ui, rect, pointer_inside);
            });

        self.details_panel(ctx);
        self.shell_prompt(ctx);

        // The camera only settles when input stops, so repaint while anything is in flight.
        if self.loaded.is_some() {
            ctx.request_repaint();
        }
    }
}

/// Toolbar icons are drawn rather than typed.
///
/// The bundled emoji font does not carry a clean glyph for every symbol wanted here, and a missing
/// glyph renders as a box. Vector shapes render identically on every machine and scale with the
/// UI, so the toolbar never depends on font coverage.
mod icon {
    use eframe::egui::{pos2, Color32, Painter, Rect, Shape, Stroke};

    pub fn folder(p: &Painter, r: Rect, c: Color32) {
        let s = Stroke::new(1.5_f32, c);
        let tab_y = r.top() + r.height() * 0.18;
        let pts = vec![
            pos2(r.left(), r.bottom()),
            pos2(r.left(), tab_y),
            pos2(r.left() + r.width() * 0.38, tab_y),
            pos2(r.left() + r.width() * 0.50, r.top()),
            pos2(r.right(), r.top()),
            pos2(r.right(), r.bottom()),
        ];
        p.add(Shape::closed_line(pts, s));
    }

    /// Four corner brackets, the standard fit-to-bounds mark.
    pub fn fit(p: &Painter, r: Rect, c: Color32) {
        let s = Stroke::new(1.5_f32, c);
        let n = r.width().min(r.height()) * 0.32;
        for (cx, cy, dx, dy) in [
            (r.left(), r.top(), 1.0, 1.0),
            (r.right(), r.top(), -1.0, 1.0),
            (r.left(), r.bottom(), 1.0, -1.0),
            (r.right(), r.bottom(), -1.0, -1.0),
        ] {
            p.line_segment([pos2(cx, cy), pos2(cx + n * dx, cy)], s);
            p.line_segment([pos2(cx, cy), pos2(cx, cy + n * dy)], s);
        }
    }
}

/// A filled triangle, or two bars. The whole of the playback bar's iconography.
fn draw_transport(painter: &egui::Painter, r: egui::Rect, colour: egui::Color32, playing: bool) {
    if playing {
        let w = r.width() * 0.3;
        for x in [r.left(), r.right() - w] {
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, r.top()), egui::vec2(w, r.height())),
                0.5,
                colour,
            );
        }
    } else {
        // Nudged right by an eighth: a triangle centred on its bounding box reads as sitting too
        // far left, because its visual mass is not where its box is.
        let x = r.left() + r.width() * 0.12;
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(x, r.top()),
                egui::pos2(x + r.width() * 0.9, r.center().y),
                egui::pos2(x, r.bottom()),
            ],
            colour,
            egui::Stroke::NONE,
        ));
    }
}

/// A square button carrying a drawn icon.
fn icon_button(
    ui: &mut egui::Ui,
    enabled: bool,
    tip: &str,
    draw: fn(&egui::Painter, egui::Rect, egui::Color32),
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(26.0, 24.0),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let visuals = ui.style().interact(&response);
    if enabled && (response.hovered() || response.is_pointer_button_down_on()) {
        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
    }
    let color = if enabled {
        visuals.fg_stroke.color
    } else {
        ui.visuals().weak_text_color()
    };
    draw(ui.painter(), rect.shrink(6.0), color);
    let response = response.on_hover_text(tip);
    enabled && response.clicked()
}

fn stats(ui: &mut egui::Ui, s: &Summary, bytes: u64) {
    let mut first = true;
    let mut item = |ui: &mut egui::Ui, label: &str, value: String| {
        if !first {
            ui.separator();
        }
        first = false;
        ui.weak(label);
        ui.label(value);
    };
    item(ui, "Size", human_bytes(bytes));
    if s.triangles > 0 {
        item(ui, "Triangles", group(s.triangles));
    }
    if s.vertices > 0 {
        item(ui, "Vertices", group(s.vertices));
    }
    if s.points > 0 {
        item(ui, "Points", group(s.points));
    }
    if s.splats > 0 {
        item(ui, "Splats", group(s.splats));
    }
    if s.voxels > 0 {
        item(ui, "Voxels", group(s.voxels));
    }
    if s.pixels > 0 {
        item(ui, "Pixels", group(s.pixels));
    }
}

fn empty_state(ui: &mut egui::Ui, size: egui::Vec2) {
    let centre = ui.max_rect().center();
    let box_size = 44.0;
    let colour = egui::Color32::from_rgb(96, 104, 114);
    icon::folder(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(centre.x, centre.y - 26.0),
            egui::vec2(box_size, box_size * 0.82),
        ),
        colour,
    );
    ui.painter().text(
        egui::pos2(centre.x, centre.y + 26.0),
        egui::Align2::CENTER_CENTER,
        "Drop a file, or press Ctrl+O.",
        egui::FontId::proportional(13.0),
        colour,
    );
    let _ = size;
}

/// Thousands separators, so an eight-digit splat count stays readable.
fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1 000");
        assert_eq!(group(1_920_000), "1 920 000");
        assert_eq!(group(34_000_000), "34 000 000");
    }

    #[test]
    fn byte_sizes_scale() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(49_700_000), "47.4 MB");
    }
}
