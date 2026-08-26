//! Capture cameras.
//!
//! Not a viewport camera — this is the *recorded* rig that produced a capture. COLMAP sparse
//! models and `transforms.json` both carry one, and drawing it is a genuinely useful feature that
//! costs almost nothing: it turns "this checkpoint is unopenable" into "here is the rig that shot
//! it, and here are the sparse points."

use crate::Mat4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Projection {
    Perspective {
        /// Vertical field of view, radians.
        yfov: f32,
        aspect: Option<f32>,
        znear: f32,
        zfar: Option<f32>,
    },
    Orthographic {
        xmag: f32,
        ymag: f32,
        znear: f32,
        zfar: f32,
    },
}

#[derive(Debug, Clone)]
pub struct CameraRig {
    pub name: String,
    /// Camera-to-world, column-major, matching glTF node transforms.
    pub transform: Mat4,
    pub projection: Projection,
    /// Source image dimensions in pixels, where known. COLMAP records these; a viewport camera has
    /// no equivalent.
    pub resolution: Option<(u32, u32)>,
    /// Path or identifier of the image this camera shot, if the source recorded one.
    pub image: Option<String>,
}

impl CameraRig {
    /// A sensible perspective default, for sources that give a pose but no intrinsics.
    pub fn perspective(name: impl Into<String>, transform: Mat4) -> Self {
        CameraRig {
            name: name.into(),
            transform,
            projection: Projection::Perspective {
                yfov: std::f32::consts::FRAC_PI_4,
                aspect: None,
                znear: 0.01,
                zfar: None,
            },
            resolution: None,
            image: None,
        }
    }

    /// Convert a focal length in pixels to a vertical FOV. COLMAP and `transforms.json` both store
    /// intrinsics this way, so every rig importer needs it.
    pub fn yfov_from_focal(focal_px: f32, height_px: f32) -> f32 {
        if focal_px <= 0.0 || height_px <= 0.0 {
            return std::f32::consts::FRAC_PI_4;
        }
        2.0 * (height_px * 0.5 / focal_px).atan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IDENTITY;

    #[test]
    fn focal_to_fov_is_symmetric_about_the_45_degree_case() {
        // focal == height/2 means a 90 degree vertical FOV.
        let fov = CameraRig::yfov_from_focal(500.0, 1000.0);
        assert!((fov - std::f32::consts::FRAC_PI_2).abs() < 1e-5, "{fov}");
    }

    #[test]
    fn degenerate_intrinsics_fall_back_rather_than_producing_nan() {
        assert_eq!(
            CameraRig::yfov_from_focal(0.0, 1000.0),
            std::f32::consts::FRAC_PI_4
        );
        assert_eq!(
            CameraRig::yfov_from_focal(500.0, 0.0),
            std::f32::consts::FRAC_PI_4
        );
    }

    #[test]
    fn default_rig_is_perspective() {
        let c = CameraRig::perspective("cam0", IDENTITY);
        assert!(matches!(c.projection, Projection::Perspective { .. }));
    }
}
