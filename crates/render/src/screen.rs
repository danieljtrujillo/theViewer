//! The two picture paths: a flat image fitted to the window, and a panorama around the viewer.
//!
//! All of the arithmetic lives here rather than in the shader so that it can be tested. Fitting
//! and framing bugs are the kind that look almost right: a picture 3% too large, a panorama that
//! drifts as you turn, and staring at a screenshot is a poor way to find them.

use bytemuck::{Pod, Zeroable};
use glam::Vec3;
use viewer_scene::{Screen, ScreenProjection, StereoLayout};

use crate::camera::Camera;

/// Matches `ScreenUniform` in `screen.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct ScreenUniform {
    pub rect: [f32; 4],
    pub eye: [f32; 4],
    pub right: [f32; 4],
    pub up: [f32; 4],
    pub forward: [f32; 4],
    pub params: [f32; 4],
}

/// How the flat picture is currently positioned. Zoom 1.0 means "fitted", not "one texel per
/// pixel": fit is what someone opening a photograph wants, and 1:1 is a keystroke away.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatView {
    pub zoom: f32,
    /// Pan, in units of half the viewport. Zero is centred.
    pub offset: [f32; 2],
    /// Which eye of a stereo pair to show. Flat displays have one.
    pub right_eye: bool,
}

impl Default for FlatView {
    fn default() -> Self {
        FlatView {
            zoom: 1.0,
            offset: [0.0, 0.0],
            right_eye: false,
        }
    }
}

impl FlatView {
    pub const MIN_ZOOM: f32 = 0.05;
    pub const MAX_ZOOM: f32 = 64.0;

    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        // Panning is only meaningful while the picture overflows the window. Snapping back at fit
        // is what stops a zoomed-out picture from sitting off in a corner.
        if self.zoom <= 1.0 {
            self.offset = [0.0, 0.0];
        }
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        if self.zoom <= 1.0 {
            return;
        }
        self.offset[0] += dx;
        self.offset[1] += dy;
        self.clamp_offset();
    }

    /// Keep at least the middle of the picture on screen. Without this a fast drag throws the
    /// image off the edge and there is no obvious way back.
    fn clamp_offset(&mut self) {
        let limit = (self.zoom - 1.0).max(0.0);
        for v in &mut self.offset {
            *v = v.clamp(-limit, limit);
        }
    }

    pub fn reset(&mut self) {
        let eye = self.right_eye;
        *self = FlatView::default();
        self.right_eye = eye;
    }
}

/// Zoom at which one texel covers one physical pixel, given the fit already applied.
///
/// The fit maps the picture's longest relevant dimension onto the viewport, so 1:1 is the ratio
/// between the pixels the fit produced and the pixels the picture has.
pub fn zoom_for_one_to_one(image: (u32, u32), viewport: (f32, f32)) -> f32 {
    let (scale_x, scale_y) = fit_scale(aspect(image), viewport);
    let fitted_width = scale_x * viewport.0;
    let fitted_height = scale_y * viewport.1;
    let by_width = image.0 as f32 / fitted_width.max(1e-3);
    let by_height = image.1 as f32 / fitted_height.max(1e-3);
    by_width
        .max(by_height)
        .clamp(FlatView::MIN_ZOOM, FlatView::MAX_ZOOM)
}

fn aspect((w, h): (u32, u32)) -> f32 {
    w.max(1) as f32 / h.max(1) as f32
}

/// Fraction of the viewport the fitted picture occupies on each axis.
///
/// Exactly one of the two is 1.0: a picture wider than the window fills its width, a taller one
/// fills its height. Letterboxing, never cropping.
fn fit_scale(image_aspect: f32, viewport: (f32, f32)) -> (f32, f32) {
    let viewport_aspect = viewport.0 / viewport.1.max(1e-3);
    if image_aspect >= viewport_aspect {
        (1.0, viewport_aspect / image_aspect.max(1e-3))
    } else {
        (image_aspect / viewport_aspect.max(1e-3), 1.0)
    }
}

impl ScreenUniform {
    pub fn new(screen: &Screen, view: &FlatView, camera: &Camera, viewport: (f32, f32)) -> Self {
        match screen.projection {
            ScreenProjection::Flat => Self::flat(screen, view, viewport),
            ScreenProjection::Equirect { horizontal_fov } => {
                Self::panorama(screen, view, camera, viewport, horizontal_fov)
            }
        }
    }

    fn flat(screen: &Screen, view: &FlatView, viewport: (f32, f32)) -> Self {
        let (sx, sy) = fit_scale(screen.aspect(), viewport);
        let z = view.zoom.clamp(FlatView::MIN_ZOOM, FlatView::MAX_ZOOM);
        ScreenUniform {
            rect: [sx * z, sy * z, view.offset[0], view.offset[1]],
            eye: eye_transform(screen.stereo, view.right_eye),
            right: [1.0, 0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0, 0.0],
            forward: [0.0, 0.0, -1.0, 0.0],
            params: [viewport.0 / viewport.1.max(1e-3), 0.0, 0.0, 0.0],
        }
    }

    fn panorama(
        screen: &Screen,
        view: &FlatView,
        camera: &Camera,
        viewport: (f32, f32),
        horizontal_fov: f32,
    ) -> Self {
        // The orbit camera drives this: its eye sits outside its target, so looking from the
        // centre of the sphere means looking along target - eye. Yaw and pitch then mean exactly
        // what they mean everywhere else in the app, and the distance is simply unused.
        let eye = camera.eye();
        let forward = (camera.target - eye).normalize_or(Vec3::NEG_Z);
        let right = forward.cross(Vec3::Y).normalize_or(Vec3::X);
        let up = right.cross(forward).normalize_or(Vec3::Y);
        let tan_half = (camera.fov_y * 0.5).tan();

        // Equirectangular keeps degrees per pixel constant, so the vertical arc follows from the
        // horizontal one and the shape of one eye. A 2:1 frame covering 360 spans 180 vertically;
        // a 1:1 VR180 frame covering 180 also spans 180.
        let arc_h = horizontal_fov.to_radians();
        let arc_v = arc_h / screen.aspect().max(1e-3);
        let full_turn = if horizontal_fov >= 359.9 { 1.0 } else { 0.0 };

        ScreenUniform {
            rect: [1.0, 1.0, 0.0, 0.0],
            eye: eye_transform(screen.stereo, view.right_eye),
            right: [right.x, right.y, right.z, tan_half],
            up: [up.x, up.y, up.z, 0.0],
            forward: [forward.x, forward.y, forward.z, 0.0],
            params: [
                viewport.0 / viewport.1.max(1e-3),
                arc_h,
                arc_v.max(1e-3),
                full_turn,
            ],
        }
    }
}

/// Texture-space scale and offset that isolates one eye.
fn eye_transform(stereo: StereoLayout, right_eye: bool) -> [f32; 4] {
    let (scale, _) = stereo.left_eye_uv();
    let offset = match (stereo, right_eye) {
        (StereoLayout::Mono, _) => [0.0, 0.0],
        (StereoLayout::OverUnder, false) => [0.0, 0.0],
        (StereoLayout::OverUnder, true) => [0.0, 0.5],
        (StereoLayout::SideBySide, false) => [0.0, 0.0],
        (StereoLayout::SideBySide, true) => [0.5, 0.0],
    };
    [scale[0], scale[1], offset[0], offset[1]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_screen(w: u32, h: u32) -> Screen {
        Screen::flat(0, w, h)
    }

    #[test]
    fn fitting_letterboxes_rather_than_cropping() {
        // A 2:1 picture in a 1:1 window fills the width and leaves bars top and bottom.
        let (sx, sy) = fit_scale(2.0, (1000.0, 1000.0));
        assert_eq!(sx, 1.0);
        assert_eq!(sy, 0.5);

        // A 1:2 picture in the same window fills the height.
        let (sx, sy) = fit_scale(0.5, (1000.0, 1000.0));
        assert_eq!(sx, 0.5);
        assert_eq!(sy, 1.0);

        // Matching aspects fill both axes exactly, with no sliver of background.
        let (sx, sy) = fit_scale(16.0 / 9.0, (1600.0, 900.0));
        assert!(
            (sx - 1.0).abs() < 1e-5 && (sy - 1.0).abs() < 1e-5,
            "{sx} {sy}"
        );
    }

    #[test]
    fn a_fitted_picture_never_overflows_the_viewport() {
        for (iw, ih) in [(4000u32, 3000u32), (100, 8000), (8000, 100), (1, 1)] {
            for viewport in [(1920.0, 1080.0), (600.0, 900.0), (1000.0, 1000.0)] {
                let u = ScreenUniform::flat(&flat_screen(iw, ih), &FlatView::default(), viewport);
                assert!(u.rect[0] <= 1.0 + 1e-5, "{iw}x{ih} in {viewport:?}");
                assert!(u.rect[1] <= 1.0 + 1e-5, "{iw}x{ih} in {viewport:?}");
                // ...and it touches at least one edge, or the fit left the picture too small.
                assert!(
                    (u.rect[0] - 1.0).abs() < 1e-5 || (u.rect[1] - 1.0).abs() < 1e-5,
                    "{iw}x{ih} in {viewport:?} filled neither axis"
                );
            }
        }
    }

    #[test]
    fn zooming_out_to_fit_recentres_the_picture() {
        let mut v = FlatView::default();
        v.zoom_by(4.0);
        v.pan(0.5, 0.5);
        assert_ne!(v.offset, [0.0, 0.0]);
        v.zoom_by(0.1);
        assert_eq!(v.offset, [0.0, 0.0], "a fitted picture must sit centred");
    }

    #[test]
    fn panning_cannot_throw_the_picture_off_the_screen() {
        let mut v = FlatView::default();
        v.zoom_by(3.0);
        for _ in 0..100 {
            v.pan(1.0, -1.0);
        }
        assert!(
            v.offset[0] <= 2.0 + 1e-5 && v.offset[1] >= -2.0 - 1e-5,
            "{:?}",
            v.offset
        );
    }

    #[test]
    fn zoom_is_clamped_at_both_ends() {
        let mut v = FlatView::default();
        for _ in 0..200 {
            v.zoom_by(2.0);
        }
        assert_eq!(v.zoom, FlatView::MAX_ZOOM);
        for _ in 0..400 {
            v.zoom_by(0.5);
        }
        assert_eq!(v.zoom, FlatView::MIN_ZOOM);
    }

    #[test]
    fn one_to_one_zoom_maps_a_texel_to_a_pixel() {
        // A 4000x3000 picture fitted into a 1000x1000 window is drawn 1000x750, so 1:1 is 4x.
        let z = zoom_for_one_to_one((4000, 3000), (1000.0, 1000.0));
        assert!((z - 4.0).abs() < 0.01, "{z}");
        // A picture smaller than the window is enlarged by the fit, so 1:1 zooms back out.
        let z = zoom_for_one_to_one((500, 500), (1000.0, 1000.0));
        assert!((z - 0.5).abs() < 0.01, "{z}");
    }

    #[test]
    fn a_full_panorama_declares_a_full_turn_and_a_half_turn_of_pitch() {
        let screen = Screen {
            projection: ScreenProjection::Equirect {
                horizontal_fov: 360.0,
            },
            ..flat_screen(8192, 4096)
        };
        let u = ScreenUniform::new(
            &screen,
            &FlatView::default(),
            &Camera::default(),
            (1600.0, 900.0),
        );
        assert!((u.params[1] - std::f32::consts::TAU).abs() < 1e-4);
        assert!((u.params[2] - std::f32::consts::PI).abs() < 1e-4);
        assert_eq!(u.params[3], 1.0, "a full turn must wrap rather than clip");
    }

    #[test]
    fn vr180_covers_a_half_turn_across_and_a_half_turn_down() {
        // The usual VR180 encoding: a 5760x2880 side-by-side frame whose eyes are 2880x2880, each
        // covering 180 across and 180 down. Reading the packed 2:1 frame instead of one 1:1 eye
        // would halve the vertical arc and squash everything.
        let screen = Screen {
            projection: ScreenProjection::Equirect {
                horizontal_fov: 180.0,
            },
            stereo: StereoLayout::SideBySide,
            ..flat_screen(5760, 2880)
        };
        let u = ScreenUniform::new(
            &screen,
            &FlatView::default(),
            &Camera::default(),
            (1600.0, 900.0),
        );
        assert!(
            (u.params[1] - std::f32::consts::PI).abs() < 1e-4,
            "{}",
            u.params[1]
        );
        assert!(
            (u.params[2] - std::f32::consts::PI).abs() < 1e-4,
            "{}",
            u.params[2]
        );
        assert_eq!(u.params[3], 0.0, "a half turn must not wrap");
    }

    #[test]
    fn a_360_stereo_frame_reads_as_two_full_panoramas_stacked() {
        // 4096x4096 over-under: each eye is 4096x2048, the ordinary 2:1 full panorama.
        let screen = Screen {
            projection: ScreenProjection::Equirect {
                horizontal_fov: 360.0,
            },
            stereo: StereoLayout::OverUnder,
            ..flat_screen(4096, 4096)
        };
        let u = ScreenUniform::new(
            &screen,
            &FlatView::default(),
            &Camera::default(),
            (1600.0, 900.0),
        );
        assert!((u.params[1] - std::f32::consts::TAU).abs() < 1e-4);
        assert!((u.params[2] - std::f32::consts::PI).abs() < 1e-4);
        assert_eq!(u.eye[1], 0.5, "only one eye may be sampled");
    }

    #[test]
    fn the_panorama_basis_is_orthonormal_at_every_orientation() {
        let mut camera = Camera::default();
        let screen = Screen {
            projection: ScreenProjection::Equirect {
                horizontal_fov: 360.0,
            },
            ..flat_screen(8192, 4096)
        };
        for i in 0..64 {
            camera.yaw = i as f32 * 0.31;
            camera.pitch = (i as f32 * 0.11).sin() * 1.5;
            let u = ScreenUniform::new(&screen, &FlatView::default(), &camera, (1600.0, 900.0));
            let r = Vec3::from_slice(&u.right[..3]);
            let up = Vec3::from_slice(&u.up[..3]);
            let f = Vec3::from_slice(&u.forward[..3]);
            for v in [r, up, f] {
                assert!((v.length() - 1.0).abs() < 1e-4, "{v} is not unit length");
            }
            assert!(r.dot(up).abs() < 1e-4);
            assert!(r.dot(f).abs() < 1e-4);
            assert!(up.dot(f).abs() < 1e-4);
        }
    }

    #[test]
    fn stereo_selection_picks_one_eye_and_only_one() {
        assert_eq!(
            eye_transform(StereoLayout::Mono, false),
            [1.0, 1.0, 0.0, 0.0]
        );
        assert_eq!(
            eye_transform(StereoLayout::OverUnder, false),
            [1.0, 0.5, 0.0, 0.0]
        );
        assert_eq!(
            eye_transform(StereoLayout::OverUnder, true),
            [1.0, 0.5, 0.0, 0.5]
        );
        assert_eq!(
            eye_transform(StereoLayout::SideBySide, true),
            [0.5, 1.0, 0.5, 0.0]
        );
    }

    #[test]
    fn the_uniform_is_the_size_the_shader_expects() {
        assert_eq!(std::mem::size_of::<ScreenUniform>(), 96);
    }
}
