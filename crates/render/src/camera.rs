//! Orbit camera.

use glam::{Mat4, Vec3};
use viewer_scene::Aabb;

#[derive(Debug, Clone)]
pub struct Camera {
    pub target: Vec3,
    pub distance: f32,
    /// Radians, rotation about the up axis.
    pub yaw: f32,
    /// Radians, clamped just inside the poles to keep the up vector stable.
    pub pitch: f32,
    pub fov_y: f32,
    pub znear: f32,
    pub zfar: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            target: Vec3::ZERO,
            distance: 4.0,
            yaw: 0.6,
            pitch: 0.4,
            fov_y: 45f32.to_radians(),
            znear: 0.01,
            zfar: 1000.0,
        }
    }
}

const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.01;

impl Camera {
    /// Position the camera so the box fills the view.
    pub fn frame(&mut self, bounds: &Aabb) {
        if bounds.is_empty() {
            *self = Camera::default();
            return;
        }
        let c = bounds.center();
        self.target = Vec3::from(c);
        let radius = bounds.radius().max(1e-4);
        self.distance = radius / (self.fov_y * 0.5).sin() * 1.15;
        self.znear = (self.distance * 0.001).max(1e-4);
        self.zfar = self.distance * 100.0;
    }

    pub fn eye(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        self.target + Vec3::new(cp * sy, sp, cp * cy) * self.distance
    }

    pub fn view(&self) -> Mat4 {
        glam::camera::rh::view::look_at_mat4(self.eye(), self.target, Vec3::Y)
    }

    /// wgpu clips depth to 0..1, which is the DirectX convention rather than the OpenGL -1..1 one.
    pub fn proj(&self, aspect: f32) -> Mat4 {
        glam::camera::rh::proj::directx::perspective(
            self.fov_y,
            aspect.max(1e-4),
            self.znear,
            self.zfar,
        )
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        self.proj(aspect) * self.view()
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx;
        self.pitch = (self.pitch + dy).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Multiplicative so the step feels constant at every scale.
    pub fn zoom(&mut self, delta: f32) {
        self.distance = (self.distance * (1.0 - delta * 0.1)).clamp(1e-4, 1e7);
    }

    /// Pan in the camera plane, scaled by distance so the grab point tracks the cursor.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let view = self.view();
        let right = Vec3::new(view.x_axis.x, view.y_axis.x, view.z_axis.x);
        let up = Vec3::new(view.x_axis.y, view.y_axis.y, view.z_axis.y);
        let scale = self.distance * (self.fov_y * 0.5).tan() * 2.0;
        self.target += (-right * dx + up * dy) * scale;
    }

    /// Vertical focal length in pixels. The splat projection needs this to size its screen-space
    /// footprint, and it must agree with the projection matrix exactly.
    pub fn focal(&self, viewport: (f32, f32)) -> (f32, f32) {
        let fy = viewport.1 * 0.5 / (self.fov_y * 0.5).tan();
        (fy, fy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_an_empty_box_falls_back_to_the_default_camera() {
        let mut c = Camera {
            distance: 999.0,
            ..Default::default()
        };
        c.frame(&Aabb::EMPTY);
        assert_eq!(c.distance, Camera::default().distance);
    }

    #[test]
    fn framing_sets_distance_from_the_bounding_radius() {
        let mut c = Camera::default();
        let b = Aabb {
            min: [-1.0; 3],
            max: [1.0; 3],
        };
        c.frame(&b);
        assert_eq!(c.target, Vec3::ZERO);
        assert!(c.distance > b.radius(), "camera must sit outside the box");
    }

    #[test]
    fn pitch_never_reaches_the_pole() {
        let mut c = Camera::default();
        for _ in 0..1000 {
            c.orbit(0.0, 1.0);
        }
        assert!(c.pitch < std::f32::consts::FRAC_PI_2);
        for _ in 0..2000 {
            c.orbit(0.0, -1.0);
        }
        assert!(c.pitch > -std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn zoom_stays_positive_under_repeated_input() {
        let mut c = Camera::default();
        for _ in 0..500 {
            c.zoom(1.0);
        }
        assert!(c.distance > 0.0);
        assert!(c.distance.is_finite());
    }

    #[test]
    fn eye_sits_at_the_requested_distance_from_the_target() {
        let c = Camera {
            target: Vec3::new(5.0, 6.0, 7.0),
            distance: 3.0,
            ..Default::default()
        };
        assert!((c.eye().distance(c.target) - 3.0).abs() < 1e-4);
    }
}
