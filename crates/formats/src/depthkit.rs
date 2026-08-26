//! Depthkit "combined per pixel" RGBD video.
//!
//! One ordinary H.264 file carrying two pictures stacked in each frame — the colour image, and the
//! same view with depth painted as a rainbow — plus a small JSON sidecar holding the depth
//! camera's intrinsics and its near and far clip. Nothing proprietary is involved: an ordinary
//! video decoder plus an unprojection gets you a moving surface, which is why `docs/FORMATS.md`
//! §E.4 calls it the best value-per-effort in the whole volumetric section.
//!
//! The decode follows Depthkit's own shader, and the three gates below were established
//! empirically against a real capture set (`G:\StableAudio\4 - Volumetric Interop\ares`,
//! `docs/rgbd-rebuild-pipeline.md`). Each of them fixes an artefact that otherwise looks like a
//! plausible-but-wrong surface rather than an obvious failure:
//!
//! 1. **Saturation gate.** Hue lives in the chroma planes, which H.264 stores at quarter
//!    resolution. Along every silhouette the decoder interpolates between the subject's hue and
//!    the background's, inventing depths that never existed. Grey pixels are not depths; requiring
//!    saturation ≥ 0.3 removes the halo.
//! 2. **Hue wrap guard.** Hue is a circle and depth is not. Values approaching 1.0 alias back onto
//!    0.0, so the far end of the range would read as the near end. Anything above 0.93 is dropped.
//! 3. **Grazing triangles.** A quad seen nearly edge-on spans a huge depth range and renders as a
//!    stretched ribbon hanging off the silhouette. Depthkit's own renderer discards these, and so
//!    must this one.

use std::path::{Path, PathBuf};

use viewer_io::FormatId;
use viewer_media::Video;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{
    Aabb, AlphaMode, Image, Material, MeshFrame, Node, Payload, Scene, Timeline,
};

use crate::{Frame, FrameSource, ImportError, Opened};

/// Grid decimation. One vertex per this many depth pixels, on each axis.
///
/// At full resolution an 848x480 depth image is 407,000 vertices and 811,000 triangles *per
/// frame*, which is 19 MB of vertex upload thirty times a second. Halving each axis quarters that
/// and costs almost nothing visually, because the depth itself is far smoother than its sampling.
const STRIDE: usize = 2;

/// Minimum chroma saturation for a pixel to be believed. See gate 1 above.
const MIN_SATURATION: f32 = 0.3;
/// Hue above this is too close to the wrap point to be trusted. See gate 2 above.
const MAX_HUE: f32 = 0.93;
/// `V^6 > 0.51`, Depthkit's own validity test, precomputed as a threshold on V.
const MIN_VALUE: f32 = 0.897_684_4;
/// Smallest absolute z component of a face normal that still gets drawn. See gate 3 above.
const MIN_FACING: f32 = 0.2;
/// Largest depth difference across one quad, as a fraction of the near-to-far range. Beyond this
/// the quad spans an occlusion boundary and is not a surface at all.
const MAX_QUAD_SPREAD: f32 = 0.06;

/// The depth camera, as the sidecar describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Intrinsics {
    pub width: u32,
    pub height: u32,
    pub focal: (f32, f32),
    pub principal: (f32, f32),
    pub near: f32,
    pub far: f32,
}

impl Intrinsics {
    fn is_plausible(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.focal.0 > 1.0
            && self.focal.1 > 1.0
            && self.far > self.near
            && self.near >= 0.0
    }
}

/// Where the two pictures sit within one video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Depth above colour, or colour above depth.
    Stacked { depth_on_top: bool },
    /// Depth left of colour, or the reverse.
    SideBySide { depth_on_left: bool },
}

/// The sidecar that turns a video into a capture: `<name>.txt`, or `<name>_metadata.json`.
///
/// Both spellings are Depthkit's own — the exporter changed between versions and both are still
/// in circulation, frequently in the same folder.
pub fn sidecar_for(video: &Path) -> Option<PathBuf> {
    let stem = video.file_stem()?.to_str()?;
    let dir = video.parent()?;
    let candidates = [
        dir.join(format!("{stem}.txt")),
        dir.join(format!("{stem}_metadata.json")),
        dir.join(format!("{stem}.json")),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// Whether this video is a Depthkit capture, which is a question only the file system can answer:
/// the container is an ordinary MP4 and the evidence is the file sitting next to it.
pub fn detect(video: &Path) -> Option<Intrinsics> {
    let text = std::fs::read_to_string(sidecar_for(video)?).ok()?;
    parse_sidecar(&text)
}

pub fn parse_sidecar(text: &str) -> Option<Intrinsics> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let pair = |key: &str| -> Option<(f32, f32)> {
        let v = json.get(key)?;
        Some((number(v.get("x")?)?, number(v.get("y")?)?))
    };
    let size = pair("depthImageSize")?;
    let intrinsics = Intrinsics {
        width: size.0 as u32,
        height: size.1 as u32,
        focal: pair("depthFocalLength")?,
        principal: pair("depthPrincipalPoint")?,
        near: number(json.get("nearClip")?)?,
        far: number(json.get("farClip")?)?,
    };
    intrinsics.is_plausible().then_some(intrinsics)
}

/// Depthkit writes numbers as numbers in some versions and as strings in others.
fn number(value: &serde_json::Value) -> Option<f32> {
    value
        .as_f64()
        .map(|v| v as f32)
        .or_else(|| value.as_str()?.trim().parse().ok())
}

/// How the two pictures are arranged in a frame of the given size.
pub fn layout_for(frame: (u32, u32), depth: (u32, u32)) -> Option<Layout> {
    if frame.0 == depth.0 && frame.1 == depth.1 * 2 {
        return Some(Layout::Stacked { depth_on_top: true });
    }
    if frame.1 == depth.1 && frame.0 == depth.0 * 2 {
        return Some(Layout::SideBySide {
            depth_on_left: true,
        });
    }
    None
}

/// Which half is the rainbow.
///
/// Decided from the pixels rather than assumed, because the exporter's order changed between
/// versions and reading it the wrong way round unprojects the *colour* image as depth — which
/// produces a surface, just not one that corresponds to anything.
fn depth_is_first(rgba: &[u8], frame: (u32, u32), layout: Layout) -> bool {
    let (w, h) = (frame.0 as usize, frame.1 as usize);
    let saturation_of = |x0: usize, y0: usize, x1: usize, y1: usize| -> f32 {
        let mut total = 0.0f32;
        let mut count = 0u32;
        // A coarse census: a few thousand samples settle this far more cheaply than every pixel.
        let step_x = ((x1 - x0) / 64).max(1);
        let step_y = ((y1 - y0) / 64).max(1);
        for y in (y0..y1).step_by(step_y) {
            for x in (x0..x1).step_by(step_x) {
                let i = (y * w + x) * 4;
                let Some(px) = rgba.get(i..i + 3) else {
                    continue;
                };
                let max = px.iter().copied().max().unwrap_or(0) as f32;
                let min = px.iter().copied().min().unwrap_or(0) as f32;
                if max > 8.0 {
                    total += (max - min) / max;
                    count += 1;
                }
            }
        }
        if count == 0 {
            0.0
        } else {
            total / count as f32
        }
    };

    match layout {
        Layout::Stacked { .. } => {
            saturation_of(0, 0, w, h / 2) > saturation_of(0, h / 2, w, h)
        }
        Layout::SideBySide { .. } => {
            saturation_of(0, 0, w / 2, h) > saturation_of(w / 2, 0, w, h)
        }
    }
}

/// Hue and validity of one depth pixel, following Depthkit's shader.
///
/// Returns `None` for any pixel the three gates reject.
fn depth_hue(r: u8, g: u8, b: u8) -> Option<f32> {
    let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    if max < MIN_VALUE {
        return None;
    }
    let delta = max - min;
    if delta / max < MIN_SATURATION {
        return None;
    }

    let hue = if max == rf {
        (gf - bf) / delta
    } else if max == gf {
        2.0 + (bf - rf) / delta
    } else {
        4.0 + (rf - gf) / delta
    };
    let hue = (hue / 6.0).rem_euclid(1.0);
    (hue <= MAX_HUE).then_some(hue)
}

pub fn import(path: &Path, intrinsics: Intrinsics) -> Result<Opened, ImportError> {
    let format = FormatId::Depthkit;
    let video = Video::open(path).map_err(|e| ImportError::Malformed {
        format: format.clone(),
        reason: e.to_string(),
    })?;
    let info = video.info().clone();

    let layout = layout_for(
        (info.width, info.height),
        (intrinsics.width, intrinsics.height),
    )
    .ok_or_else(|| ImportError::Malformed {
        format: format.clone(),
        reason: format!(
            "a {}x{} frame does not hold two {}x{} images",
            info.width, info.height, intrinsics.width, intrinsics.height
        ),
    })?;

    let mut scene = Scene::new();
    let image = scene.push_dynamic_image(Image {
        name: "frame".into(),
        width: info.width.max(1),
        height: info.height.max(1),
        rgba: vec![0; info.width.max(1) as usize * info.height.max(1) as usize * 4],
    });
    let material = scene.push_material(Material {
        name: "depthkit".into(),
        base_color: [1.0, 1.0, 1.0, 1.0],
        base_color_texture: Some(image),
        alpha_mode: AlphaMode::Opaque,
        double_sided: true,
        ..Default::default()
    });

    // The mesh starts empty and is replaced by the first decoded frame. Its declared bounds are
    // the view frustum between the near and far clip, so the camera frames sensibly before any
    // geometry exists.
    let bounds = frustum_bounds(&intrinsics);
    let node = scene.push_root(Node::with_payload(
        "capture",
        Payload::TriMesh(TriMesh {
            attributes: vec![Attribute::new(
                Semantic::Position,
                ComponentType::F32,
                3,
                0,
                Vec::new(),
            )
            .unwrap()],
            indices: Indices::U32(Vec::new()),
            bounds,
            material: Some(material),
        }),
    ));
    scene.dynamic_meshes.push(node);

    let timeline = Timeline {
        frame_count: info.frame_count(),
        fps: info.fps,
        duration_us: info.duration_us,
        seekable: true,
    };
    scene.timeline = Some(timeline.clone());

    scene.set_meta("format", "Depthkit combined-per-pixel");
    if !info.codec.is_empty() {
        scene.set_meta("codec", &info.codec);
    }
    scene.set_meta(
        "depth image",
        format!("{} x {}", intrinsics.width, intrinsics.height),
    );
    scene.set_meta(
        "layout",
        match layout {
            Layout::Stacked { .. } => "colour and depth stacked",
            Layout::SideBySide { .. } => "colour and depth side by side",
        },
    );
    scene.set_meta(
        "depth range",
        format!("{:.3} to {:.3} m", intrinsics.near, intrinsics.far),
    );
    scene.set_meta(
        "focal length",
        format!("{:.1}, {:.1} px", intrinsics.focal.0, intrinsics.focal.1),
    );
    scene.set_meta("frames", timeline.frame_count.to_string());
    scene.set_meta("duration", crate::movie::format_duration(info.duration_us));

    Ok(Opened {
        scene,
        frames: Some(Box::new(DepthkitSource {
            video,
            format,
            intrinsics,
            layout,
            depth_first: None,
            timeline,
            served: None,
            ended: false,
            frame_size: (info.width, info.height),
        })),
    })
}

/// The box the capture can possibly occupy: the depth camera's frustum, clipped near and far.
fn frustum_bounds(k: &Intrinsics) -> Aabb {
    let half_x = (k.width as f32 / k.focal.0) * k.far * 0.5;
    let half_y = (k.height as f32 / k.focal.1) * k.far * 0.5;
    Aabb {
        min: [-half_x, -half_y, -k.far],
        max: [half_x, half_y, -k.near],
    }
}

struct DepthkitSource {
    video: Video,
    format: FormatId,
    intrinsics: Intrinsics,
    layout: Layout,
    /// Decided from the first frame's pixels, then kept.
    depth_first: Option<bool>,
    timeline: Timeline,
    served: Option<u32>,
    ended: bool,
    frame_size: (u32, u32),
}

impl FrameSource for DepthkitSource {
    fn timeline(&self) -> Timeline {
        self.timeline.clone()
    }

    fn frame_at(&mut self, pts_us: u64) -> Result<Option<Frame>, ImportError> {
        let index = self
            .timeline
            .frame_at(pts_us as f64 / 1_000_000.0)
            .min(self.timeline.frame_count.saturating_sub(1));
        if self.served == Some(index) || self.ended {
            return Ok(None);
        }

        // Backwards, or a long way forward, is a seek; otherwise decode onward.
        if self
            .served
            .is_some_and(|last| index < last || index > last + 30)
            || (self.served.is_none() && index > 0)
        {
            self.video
                .seek(pts_us)
                .map_err(|e| self.media_error(e))?;
        }

        let Some(video_frame) = self
            .video
            .next_frame()
            .map_err(|e| self.media_error(e))?
        else {
            self.ended = true;
            return Ok(None);
        };
        self.frame_size = (video_frame.width, video_frame.height);

        let depth_first = *self.depth_first.get_or_insert_with(|| {
            depth_is_first(&video_frame.rgba, self.frame_size, self.layout)
        });
        let mesh = unproject(
            &video_frame.rgba,
            self.frame_size,
            &self.intrinsics,
            self.layout,
            depth_first,
        );

        self.served = Some(index);
        Ok(Some(Frame {
            index,
            pts_us: video_frame.pts_us,
            image: Some(Image {
                name: "frame".into(),
                width: video_frame.width,
                height: video_frame.height,
                rgba: video_frame.rgba,
            }),
            mesh: Some(mesh),
        }))
    }
}

impl DepthkitSource {
    fn media_error(&self, e: viewer_media::MediaError) -> ImportError {
        ImportError::Malformed {
            format: self.format.clone(),
            reason: e.to_string(),
        }
    }
}

/// Turn one frame into a surface.
///
/// Every grid position produces a vertex whether or not its depth is valid, and validity decides
/// only which *triangles* are emitted. Keeping the vertex grid dense and regular is what lets the
/// index arithmetic stay simple, and the unused vertices cost a few hundred kilobytes against a
/// rewrite of the addressing.
pub fn unproject(
    rgba: &[u8],
    frame: (u32, u32),
    k: &Intrinsics,
    layout: Layout,
    depth_first: bool,
) -> MeshFrame {
    let (fw, fh) = (frame.0 as usize, frame.1 as usize);
    let (dw, dh) = (k.width as usize, k.height as usize);

    // Where each half begins, in frame pixels.
    let (depth_origin, colour_origin) = match layout {
        Layout::Stacked { .. } => {
            let second = (0usize, dh);
            if depth_first {
                ((0, 0), second)
            } else {
                (second, (0, 0))
            }
        }
        Layout::SideBySide { .. } => {
            let second = (dw, 0usize);
            if depth_first {
                ((0, 0), second)
            } else {
                (second, (0, 0))
            }
        }
    };

    let cols = dw.div_ceil(STRIDE);
    let rows = dh.div_ceil(STRIDE);
    let span = k.far - k.near;

    let mut positions = vec![[0.0f32; 3]; cols * rows];
    let mut uvs = vec![[0.0f32; 2]; cols * rows];
    let mut valid = vec![false; cols * rows];
    let mut depths = vec![0.0f32; cols * rows];
    let mut bounds = Aabb::EMPTY;

    for row in 0..rows {
        let y = row * STRIDE;
        for col in 0..cols {
            let x = col * STRIDE;
            let i = row * cols + col;

            // Texture coordinates point into the *colour* half, and are set for every grid
            // position regardless of validity so that a vertex never samples position (0, 0).
            uvs[i] = [
                (colour_origin.0 + x) as f32 / fw as f32,
                (colour_origin.1 + y) as f32 / fh as f32,
            ];

            let px = ((depth_origin.1 + y) * fw + depth_origin.0 + x) * 4;
            let Some(texel) = rgba.get(px..px + 3) else {
                continue;
            };
            let Some(hue) = depth_hue(texel[0], texel[1], texel[2]) else {
                continue;
            };

            // Pinhole unprojection. Y is flipped and Z negated because the scene's camera looks
            // down -Z with +Y up, and the depth image is indexed from its top-left corner.
            let z = k.near + hue * span;
            let position = [
                (x as f32 - k.principal.0) * z / k.focal.0,
                (k.principal.1 - y as f32) * z / k.focal.1,
                -z,
            ];
            positions[i] = position;
            depths[i] = z;
            valid[i] = true;
            bounds.expand(position);
        }
    }

    // Two triangles per quad whose four corners all survived, are not straddling an occlusion
    // boundary, and are not seen so nearly edge-on that they would render as a ribbon.
    let mut indices = Vec::new();
    let max_spread = span * MAX_QUAD_SPREAD;
    for row in 0..rows.saturating_sub(1) {
        for col in 0..cols.saturating_sub(1) {
            let a = row * cols + col;
            let b = a + 1;
            let c = a + cols;
            let d = c + 1;
            if !(valid[a] && valid[b] && valid[c] && valid[d]) {
                continue;
            }
            let lo = depths[a].min(depths[b]).min(depths[c]).min(depths[d]);
            let hi = depths[a].max(depths[b]).max(depths[c]).max(depths[d]);
            if hi - lo > max_spread {
                continue;
            }
            if !facing(&positions[a], &positions[b], &positions[c]) {
                continue;
            }
            indices.extend_from_slice(&[a as u32, c as u32, b as u32]);
            indices.extend_from_slice(&[b as u32, c as u32, d as u32]);
        }
    }

    MeshFrame {
        positions,
        normals: Vec::new(),
        uvs,
        indices,
        bounds,
    }
}

/// Whether a triangle faces the camera closely enough to be worth drawing.
fn facing(a: &[f32; 3], b: &[f32; 3], c: &[f32; 3]) -> bool {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if length < 1e-9 {
        return false;
    }
    (n[2] / length).abs() >= MIN_FACING
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidecar of a real capture, verbatim apart from its identifiers.
    const SIDECAR: &str = r#"{
        "_versionMajor" : "0",
        "_versionMinor" : "1",
        "depthFocalLength" : { "x" : 424.929, "y" : 424.929 },
        "depthImageSize" : { "x" : 848, "y" : 480 },
        "depthPrincipalPoint" : { "x" : 417.386, "y" : 243.924 },
        "farClip" : 2.61545,
        "format" : "perpixel",
        "nearClip": 1.85071,
        "clipEpsilon" : 0.00653817
    }"#;

    #[test]
    fn a_real_sidecar_parses() {
        let k = parse_sidecar(SIDECAR).expect("a Depthkit sidecar");
        assert_eq!((k.width, k.height), (848, 480));
        assert!((k.focal.0 - 424.929).abs() < 1e-3);
        assert!((k.principal.1 - 243.924).abs() < 1e-3);
        assert!((k.near - 1.85071).abs() < 1e-5);
        assert!((k.far - 2.61545).abs() < 1e-5);
    }

    #[test]
    fn numbers_written_as_strings_parse_too() {
        // Some exporter versions quote every value.
        let quoted = r#"{
            "depthFocalLength" : { "x" : "424.9", "y" : "424.9" },
            "depthImageSize" : { "x" : "848", "y" : "480" },
            "depthPrincipalPoint" : { "x" : "417", "y" : "243" },
            "farClip" : "2.6", "nearClip": "1.8"
        }"#;
        let k = parse_sidecar(quoted).expect("quoted numbers are still numbers");
        assert_eq!((k.width, k.height), (848, 480));
    }

    #[test]
    fn nonsense_sidecars_are_refused_rather_than_half_read() {
        assert_eq!(parse_sidecar("not json at all"), None);
        assert_eq!(parse_sidecar("{}"), None);
        // Far behind near is not a depth range.
        let inverted = SIDECAR.replace("2.61545", "0.5");
        assert_eq!(parse_sidecar(&inverted), None);
    }

    #[test]
    fn the_frame_layout_follows_from_the_two_sizes() {
        assert_eq!(
            layout_for((848, 960), (848, 480)),
            Some(Layout::Stacked { depth_on_top: true })
        );
        assert_eq!(
            layout_for((1696, 480), (848, 480)),
            Some(Layout::SideBySide {
                depth_on_left: true
            })
        );
        // A plain video of the same nominal size is not a capture.
        assert_eq!(layout_for((848, 480), (848, 480)), None);
        assert_eq!(layout_for((1920, 1080), (848, 480)), None);
    }

    #[test]
    fn the_three_gates_reject_what_they_are_meant_to() {
        // A saturated bright red is hue 0: the near end of the range.
        assert_eq!(depth_hue(255, 0, 0), Some(0.0));
        // Grey is not a depth, however bright: this is the chroma smear along silhouettes.
        assert_eq!(depth_hue(230, 230, 230), None);
        // Dark is not a depth either: outside the capture volume.
        assert_eq!(depth_hue(80, 0, 0), None);
        // Just short of the wrap is fine; past it is not. Hue 0.92 is a red-magenta; 0.97 is so
        // close to red that it would read as the *near* clip instead of the far one.
        let just_inside = depth_hue(255, 0, 123).expect("hue 0.92 is inside the guard");
        assert!((just_inside - 0.92).abs() < 0.01, "{just_inside}");
        assert_eq!(depth_hue(255, 0, 40), None, "hue near 1.0 aliases onto 0.0");
    }

    #[test]
    fn hue_runs_around_the_colour_wheel_in_order() {
        // Red, yellow, green, cyan, blue: 0, 1/6, 2/6, 3/6, 4/6.
        let steps = [
            (depth_hue(255, 0, 0), 0.0),
            (depth_hue(255, 255, 0), 1.0 / 6.0),
            (depth_hue(0, 255, 0), 2.0 / 6.0),
            (depth_hue(0, 255, 255), 3.0 / 6.0),
            (depth_hue(0, 0, 255), 4.0 / 6.0),
        ];
        for (got, want) in steps {
            let got = got.expect("a fully saturated primary is valid");
            assert!((got - want).abs() < 1e-4, "{got} vs {want}");
        }
    }

    #[test]
    fn a_grazing_triangle_is_dropped_and_a_facing_one_is_kept() {
        // Facing the camera: the normal is along z.
        assert!(facing(
            &[0.0, 0.0, -2.0],
            &[0.1, 0.0, -2.0],
            &[0.0, 0.1, -2.0]
        ));
        // Edge-on: the normal is perpendicular to z, which is the stretched-ribbon case.
        assert!(!facing(
            &[0.0, 0.0, -2.0],
            &[0.0, 0.0, -2.5],
            &[0.0, 0.1, -2.0]
        ));
        // Degenerate triangles have no normal at all.
        assert!(!facing(&[0.0; 3], &[0.0; 3], &[0.0; 3]));
    }

    #[test]
    fn the_frustum_bounds_span_the_declared_depth_range() {
        let k = parse_sidecar(SIDECAR).unwrap();
        let b = frustum_bounds(&k);
        assert!((b.max[2] + k.near).abs() < 1e-5);
        assert!((b.min[2] + k.far).abs() < 1e-5);
        // About a metre and a quarter across at the far plane, for a 848 px sensor at f=425.
        assert!(b.extent()[0] > 1.0 && b.extent()[0] < 10.0, "{:?}", b.extent());
    }

    /// A synthetic frame: a flat plane at mid depth in the top half, grey in the bottom.
    fn synthetic_frame(k: &Intrinsics) -> (Vec<u8>, (u32, u32)) {
        let (w, h) = (k.width as usize, k.height as usize * 2);
        let mut rgba = vec![0u8; w * h * 4];
        // Hue 1/3 is fully saturated green: exactly a third of the way from near to far.
        for y in 0..k.height as usize {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i + 1] = 255;
                rgba[i + 3] = 255;
            }
        }
        for y in k.height as usize..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = 128;
                rgba[i + 1] = 128;
                rgba[i + 2] = 128;
                rgba[i + 3] = 255;
            }
        }
        (rgba, (w as u32, h as u32))
    }

    #[test]
    fn a_flat_plane_unprojects_to_a_flat_plane_at_the_right_depth() {
        let k = parse_sidecar(SIDECAR).unwrap();
        let (rgba, frame) = synthetic_frame(&k);
        let mesh = unproject(
            &rgba,
            frame,
            &k,
            Layout::Stacked { depth_on_top: true },
            true,
        );

        let expected = k.near + (2.0 / 6.0) * (k.far - k.near);
        for p in &mesh.positions {
            assert!(
                (-p[2] - expected).abs() < 1e-3,
                "expected z = -{expected}, got {p:?}"
            );
        }
        // A full plane of valid depth becomes a full grid of quads.
        let cols = (k.width as usize).div_ceil(STRIDE);
        let rows = (k.height as usize).div_ceil(STRIDE);
        assert_eq!(mesh.positions.len(), cols * rows);
        assert_eq!(mesh.indices.len(), (cols - 1) * (rows - 1) * 6);
        assert!(!mesh.bounds.is_empty());
    }

    #[test]
    fn the_rainbow_half_is_found_from_the_pixels() {
        let k = parse_sidecar(SIDECAR).unwrap();
        let (rgba, frame) = synthetic_frame(&k);
        assert!(
            depth_is_first(&rgba, frame, Layout::Stacked { depth_on_top: true }),
            "the saturated half is the depth half"
        );
    }

    #[test]
    fn a_grey_frame_produces_no_triangles_rather_than_a_flat_wall() {
        // Every pixel fails the saturation gate, so nothing is a depth.
        let k = parse_sidecar(SIDECAR).unwrap();
        let (w, h) = (k.width as usize, k.height as usize * 2);
        let rgba = vec![200u8; w * h * 4];
        let mesh = unproject(
            &rgba,
            (w as u32, h as u32),
            &k,
            Layout::Stacked { depth_on_top: true },
            true,
        );
        assert!(mesh.indices.is_empty());
        assert!(mesh.bounds.is_empty());
    }

    #[test]
    fn a_truncated_frame_does_not_read_past_its_end() {
        let k = parse_sidecar(SIDECAR).unwrap();
        let mesh = unproject(
            &[255u8; 64],
            (k.width, k.height * 2),
            &k,
            Layout::Stacked { depth_on_top: true },
            true,
        );
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn the_sidecar_is_looked_for_under_both_of_depthkits_names() {
        // Nothing on disk here; the point is that the search does not panic on odd paths.
        assert_eq!(sidecar_for(Path::new("nonexistent.mp4")), None);
        assert_eq!(sidecar_for(Path::new("/")), None);
    }
}
