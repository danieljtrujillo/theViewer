//! Materials and images.
//!
//! Modelled on glTF's metallic-roughness set, matching the rest of the scene model. Formats that
//! predate PBR (OBJ's MTL, FBX's Lambert and Phong) map onto it on import rather than carrying a
//! second material system through the renderer.
//!
//! Images are decoded to RGBA8 here because every renderer path wants one upload format, and the
//! source encodings (PNG, JPEG, TGA, embedded FBX blobs, DICOM frames) have nothing in common
//! beyond what they decode to.

/// A decoded RGBA8 image.
#[derive(Clone)]
pub struct Image {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Exactly `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn is_consistent(&self) -> bool {
        self.rgba.len() == self.width as usize * self.height as usize * 4
    }

    /// A single-pixel white image, used where a material declares a texture that failed to load.
    /// Substituting white keeps the base colour factor visible instead of rendering the mesh black.
    pub fn white() -> Image {
        Image {
            name: "white".into(),
            width: 1,
            height: 1,
            rgba: vec![255, 255, 255, 255],
        }
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("name", &self.name)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    Opaque,
    /// Fragments below `alpha_cutoff` are discarded. What OBJ's `d` and FBX's transparency maps
    /// almost always mean in practice.
    Mask,
    Blend,
}

#[derive(Debug, Clone)]
pub struct Material {
    pub name: String,
    pub base_color: [f32; 4],
    /// Index into [`crate::Scene::images`].
    pub base_color_texture: Option<u32>,
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub alpha_mode: AlphaMode,
    pub alpha_cutoff: f32,
    /// Backface culling is off by default: scans and per-frame reconstructions carry inconsistent
    /// winding, and culling them hides half the surface.
    pub double_sided: bool,
}

impl Default for Material {
    fn default() -> Self {
        Material {
            name: String::new(),
            base_color: [0.82, 0.82, 0.85, 1.0],
            base_color_texture: None,
            metallic: 0.0,
            roughness: 0.8,
            emissive: [0.0; 3],
            alpha_mode: AlphaMode::Opaque,
            alpha_cutoff: 0.5,
            double_sided: true,
        }
    }
}

impl Material {
    pub fn named(name: impl Into<String>) -> Self {
        Material {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Convert a legacy specular exponent to a roughness value.
    ///
    /// OBJ's `Ns` and FBX's shininess both use the Blinn-Phong exponent, which runs 0 to about
    /// 1000. The mapping is the usual inverse square root; it is approximate by nature, since the
    /// two shading models do not correspond exactly.
    pub fn roughness_from_shininess(ns: f32) -> f32 {
        if ns <= 0.0 {
            return 1.0;
        }
        (2.0 / (ns.min(1024.0) + 2.0)).sqrt().clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_consistency_catches_a_truncated_buffer() {
        let mut i = Image {
            name: "t".into(),
            width: 2,
            height: 2,
            rgba: vec![0; 16],
        };
        assert!(i.is_consistent());
        i.rgba.truncate(12);
        assert!(!i.is_consistent());
    }

    #[test]
    fn the_white_placeholder_is_one_opaque_pixel() {
        let w = Image::white();
        assert!(w.is_consistent());
        assert_eq!(w.rgba, vec![255, 255, 255, 255]);
    }

    #[test]
    fn shininess_maps_to_roughness_monotonically() {
        // A higher exponent is a sharper highlight, so roughness must fall.
        let a = Material::roughness_from_shininess(1.0);
        let b = Material::roughness_from_shininess(100.0);
        let c = Material::roughness_from_shininess(1000.0);
        assert!(a > b && b > c, "{a} {b} {c}");
        assert!((0.0..=1.0).contains(&c));
    }

    #[test]
    fn a_zero_or_negative_exponent_is_fully_rough() {
        assert_eq!(Material::roughness_from_shininess(0.0), 1.0);
        assert_eq!(Material::roughness_from_shininess(-5.0), 1.0);
    }

    #[test]
    fn the_default_material_is_double_sided() {
        assert!(Material::default().double_sided);
    }
}
