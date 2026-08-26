//! The five things a node can actually contain, and the six renderer paths they feed.
//!
//! Keeping this enum small is the point. Every format in `docs/FORMATS.md`, all ~230 extensions,
//! reduces to one of these, which is what keeps the renderer at six paths instead of growing one
//! per format.

use crate::attr::{Attribute, Semantic};
use crate::Aabb;

#[derive(Debug, Clone)]
pub enum Payload {
    TriMesh(TriMesh),
    PointCloud(PointCloud),
    SplatCloud(SplatCloud),
    VolumeGrid(VolumeGrid),
    Screen(Screen),
}

impl Payload {
    pub fn bounds(&self) -> Aabb {
        match self {
            Payload::TriMesh(m) => m.bounds,
            Payload::PointCloud(p) => p.bounds,
            Payload::SplatCloud(s) => s.bounds,
            Payload::VolumeGrid(v) => v.bounds,
            Payload::Screen(s) => s.bounds(),
        }
    }

    /// Which renderer path this payload needs. The RGBD and video-sequence paths are not here:
    /// those are properties of a *sequence*, not of a single payload.
    pub fn render_path(&self) -> RenderPath {
        match self {
            Payload::TriMesh(_) => RenderPath::MeshRaster,
            Payload::PointCloud(_) => RenderPath::PointSprite,
            Payload::SplatCloud(_) => RenderPath::SortedSplat,
            Payload::VolumeGrid(_) => RenderPath::VolumeRaymarch,
            Payload::Screen(s) => match s.projection {
                ScreenProjection::Flat => RenderPath::FlatImage,
                ScreenProjection::Equirect { .. } => RenderPath::Panorama,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderPath {
    MeshRaster,
    PointSprite,
    SortedSplat,
    VolumeRaymarch,
    /// A still or moving image presented as an image rather than as geometry in a scene.
    FlatImage,
    /// The same pixels wrapped onto a sphere around the viewer.
    Panorama,
    VideoTexturedSequence,
    UnprojectedRgbd,
}

/// Index buffer width. Kept narrow where the source allows it: a 16-bit index buffer is half the
/// upload of a 32-bit one, and plenty of formats never exceed 65,535 vertices per primitive.
#[derive(Debug, Clone)]
pub enum Indices {
    None,
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl Indices {
    pub fn len(&self) -> usize {
        match self {
            Indices::None => 0,
            Indices::U16(v) => v.len(),
            Indices::U32(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct TriMesh {
    pub attributes: Vec<Attribute>,
    pub indices: Indices,
    pub bounds: Aabb,
    /// Index into a material table owned by the importer. `None` renders with the default.
    pub material: Option<u32>,
}

impl TriMesh {
    pub fn attribute(&self, semantic: &Semantic) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.semantic == *semantic)
    }

    pub fn vertex_count(&self) -> usize {
        self.attribute(&Semantic::Position).map_or(0, |a| a.count)
    }

    pub fn triangle_count(&self) -> usize {
        if self.indices.is_empty() {
            self.vertex_count() / 3
        } else {
            self.indices.len() / 3
        }
    }

    /// True when the mesh carries no normals and the renderer must derive them.
    ///
    /// Worth knowing early: STL has no usable topology at all, and `.4ds` frames arrive with
    /// per-vertex UVs but no normals.
    pub fn needs_derived_normals(&self) -> bool {
        self.attribute(&Semantic::Normal).is_none()
    }
}

#[derive(Debug, Clone)]
pub struct PointCloud {
    pub attributes: Vec<Attribute>,
    pub count: usize,
    pub bounds: Aabb,
}

impl PointCloud {
    pub fn attribute(&self, semantic: &Semantic) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.semantic == *semantic)
    }
}

/// A 3D Gaussian splat cloud.
///
/// Attributes are stored with `KHR_gaussian_splatting` semantics (see [`Semantic`]), and stay in
/// source precision: SPZ's int24 positions and `u8` scales reach the GPU as int24 and `u8`.
#[derive(Debug, Clone)]
pub struct SplatCloud {
    pub attributes: Vec<Attribute>,
    pub count: usize,
    pub bounds: Aabb,
    /// 0..=4. SPZ 4 raised the ceiling to 4; the Khronos extension stops at 3.
    pub sh_degree: u8,
    /// Set by SPZ's `flags` bit 0. Changes the rasterizer's covariance dilation.
    pub antialiased: bool,
    /// Safe orbit bounds if the source supplied them: SPZ 4 vendor extension `0xADBE0002` from
    /// Adobe carries exactly this, and honouring it hands the viewer a sane default camera for
    /// free rather than framing the raw AABB, which for splat captures is usually far too large.
    pub orbit_hint: Option<Aabb>,
}

impl SplatCloud {
    pub fn attribute(&self, semantic: &Semantic) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.semantic == *semantic)
    }

    /// Number of SH coefficient triples per splat for a given degree. This is the `shDim / 3`
    /// sequence SPZ encodes: degree 0/1/2/3 gives 0/9/24/45 bytes, i.e. 0/3/8/15 triples.
    pub const fn sh_coef_count(degree: u8) -> usize {
        match degree {
            0 => 0,
            1 => 3,
            2 => 8,
            3 => 15,
            // Degree 4 adds a further 9 triples. SPZ 4 only.
            _ => 24,
        }
    }

    /// Whether a per-frame back-to-front sort is worth doing on the GPU rather than the CPU.
    ///
    /// The threshold is deliberately low. Measured captures run 1.9 M splats for a single room and
    /// SPZ 4 targets 34 M+, so in practice this is always true for real content and the CPU path
    /// exists only for tiny test fixtures.
    pub fn needs_gpu_sort(&self) -> bool {
        self.count > 100_000
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoxelType {
    U8,
    U16,
    I16,
    F32,
}

impl VoxelType {
    pub const fn size(self) -> usize {
        match self {
            VoxelType::U8 => 1,
            VoxelType::U16 | VoxelType::I16 => 2,
            VoxelType::F32 => 4,
        }
    }
}

/// A dense scalar volume: DICOM series, NIfTI, NRRD, MetaImage, raw + header.
///
/// Sparse grids (VDB, NanoVDB) do not fit this shape and will need a sibling type once that
/// importer lands, noting it here rather than pretending one struct covers both.
#[derive(Debug, Clone)]
pub struct VolumeGrid {
    pub dims: [u32; 3],
    /// World-space size of one voxel. DICOM and NIfTI both carry real physical spacing, and
    /// ignoring it renders anatomy at the wrong aspect ratio.
    pub spacing: [f32; 3],
    pub voxel: VoxelType,
    pub bounds: Aabb,
    pub data: Vec<u8>,
    /// Default window/level for display, where the source declared one. DICOM almost always does.
    pub window: Option<(f32, f32)>,
}

impl VolumeGrid {
    pub fn voxel_count(&self) -> usize {
        self.dims[0] as usize * self.dims[1] as usize * self.dims[2] as usize
    }

    pub fn expected_bytes(&self) -> usize {
        self.voxel_count() * self.voxel.size()
    }

    /// The same cheap corruption gate the attribute buffers use: a truncated DICOM series or a
    /// raw volume with the wrong declared dimensions fails here rather than rendering garbage.
    pub fn is_consistent(&self) -> bool {
        self.data.len() == self.expected_bytes()
    }
}

/// How the pixels of a [`Screen`] map onto the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScreenProjection {
    /// An ordinary picture. Shown flat, fitted to the window, pannable and zoomable.
    Flat,
    /// Equirectangular: longitude across, latitude down, viewed from the centre of the sphere.
    ///
    /// `horizontal_fov` is 360 for a full panorama and 180 for VR180, which is the same
    /// projection covering half the sphere. Storing the angle rather than a `Full`/`Half` enum
    /// keeps GPano's `CroppedArea*` fields, which describe partial panoramas, representable.
    Equirect { horizontal_fov: f32 },
}

/// How a stereo pair is packed into one frame. Mono is by far the common case; the two stereo
/// layouts exist because VR180 and spatial video always carry both eyes in a single image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StereoLayout {
    Mono,
    /// Left eye in the top half, right eye in the bottom. What VR180 uses.
    OverUnder,
    /// Left eye in the left half, right in the right.
    SideBySide,
}

impl StereoLayout {
    /// Texture-space scale and offset that isolates the left eye. The renderer shows one eye on a
    /// flat display; showing both would be a picture of two pictures.
    pub fn left_eye_uv(self) -> ([f32; 2], [f32; 2]) {
        match self {
            StereoLayout::Mono => ([1.0, 1.0], [0.0, 0.0]),
            StereoLayout::OverUnder => ([1.0, 0.5], [0.0, 0.0]),
            StereoLayout::SideBySide => ([0.5, 1.0], [0.0, 0.0]),
        }
    }

    /// Aspect correction to undo the packing, so a 2:1 over-under VR180 frame is treated as the
    /// 1:1 half-sphere it actually is.
    pub fn unpacked_aspect(self, width: u32, height: u32) -> f32 {
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        match self {
            StereoLayout::Mono => w / h,
            StereoLayout::OverUnder => w / (h * 0.5),
            StereoLayout::SideBySide => (w * 0.5) / h,
        }
    }
}

/// Geometry for one frame of a sequence: volumetric video, a mesh-sequence folder, an animated
/// splat capture.
///
/// `indices` empty means *unchanged*. In an inter-coded stream the topology is set by the
/// keyframe and every predicted frame after it moves the same vertices, so re-uploading an index
/// buffer that did not change is both the easiest thing to get wrong here and the most expensive.
#[derive(Debug, Clone, Default)]
pub struct MeshFrame {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub bounds: Aabb,
}

impl MeshFrame {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    /// True when this frame carries its own topology rather than inheriting the previous one.
    pub fn is_keyframe(&self) -> bool {
        !self.indices.is_empty()
    }
}

/// An image shown as an image: a photograph, a video frame, a 360 panorama.
///
/// This is the payload that does *not* become geometry. A picture rendered as a textured quad in a
/// 3D scene is a picture you have to hunt for with an orbit camera; the point of a separate
/// payload is that the viewer can present it the way the format means it to be seen.
#[derive(Debug, Clone)]
pub struct Screen {
    /// Index into [`crate::Scene::images`]. For a video this is the frame currently decoded, and
    /// the player overwrites it in place.
    pub image: u32,
    pub projection: ScreenProjection,
    pub stereo: StereoLayout,
    /// Source pixel dimensions, before any stereo split.
    pub width: u32,
    pub height: u32,
}

impl Screen {
    pub fn flat(image: u32, width: u32, height: u32) -> Screen {
        Screen {
            image,
            projection: ScreenProjection::Flat,
            stereo: StereoLayout::Mono,
            width,
            height,
        }
    }

    /// Displayed aspect ratio, after undoing any stereo packing.
    pub fn aspect(&self) -> f32 {
        self.stereo.unpacked_aspect(self.width, self.height)
    }

    /// A flat picture occupies a unit-height rectangle at the origin; a panorama surrounds the
    /// viewer, so it gets the unit sphere. Neither is what the screen path actually draws: both
    /// exist so that scene bounds, framing and the inspector keep working uniformly.
    pub fn bounds(&self) -> Aabb {
        match self.projection {
            ScreenProjection::Flat => {
                let half = self.aspect().max(1e-3) * 0.5;
                Aabb {
                    min: [-half, -0.5, 0.0],
                    max: [half, 0.5, 0.0],
                }
            }
            ScreenProjection::Equirect { .. } => Aabb {
                min: [-1.0; 3],
                max: [1.0; 3],
            },
        }
    }

    pub fn is_panorama(&self) -> bool {
        matches!(self.projection, ScreenProjection::Equirect { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::{Attribute, ComponentType};

    fn positions(n: usize) -> Attribute {
        Attribute::new(
            Semantic::Position,
            ComponentType::F32,
            3,
            n,
            vec![0; n * 12],
        )
        .unwrap()
    }

    #[test]
    fn triangle_count_uses_indices_when_present() {
        let m = TriMesh {
            attributes: vec![positions(4)],
            indices: Indices::U16(vec![0, 1, 2, 0, 2, 3]),
            bounds: Aabb::EMPTY,
            material: None,
        };
        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.triangle_count(), 2);
    }

    #[test]
    fn triangle_count_falls_back_to_vertices_for_unindexed_meshes() {
        // STL, and every other format with no topology.
        let m = TriMesh {
            attributes: vec![positions(9)],
            indices: Indices::None,
            bounds: Aabb::EMPTY,
            material: None,
        };
        assert_eq!(m.triangle_count(), 3);
        assert!(m.needs_derived_normals());
    }

    #[test]
    fn sh_coef_counts_match_the_spz_byte_layout() {
        // SPZ stores shDim bytes per splat: 0 / 9 / 24 / 45 for degrees 0..=3.
        for (degree, sh_dim) in [(0u8, 0usize), (1, 9), (2, 24), (3, 45)] {
            assert_eq!(SplatCloud::sh_coef_count(degree) * 3, sh_dim);
        }
    }

    #[test]
    fn volume_consistency_catches_a_truncated_series() {
        let mut v = VolumeGrid {
            dims: [4, 4, 4],
            spacing: [1.0; 3],
            voxel: VoxelType::U16,
            bounds: Aabb::EMPTY,
            data: vec![0; 128],
            window: None,
        };
        assert_eq!(v.expected_bytes(), 128);
        assert!(v.is_consistent());
        v.data.truncate(100);
        assert!(!v.is_consistent());
    }

    #[test]
    fn each_payload_maps_to_exactly_one_render_path() {
        let m = Payload::TriMesh(TriMesh {
            attributes: vec![],
            indices: Indices::None,
            bounds: Aabb::EMPTY,
            material: None,
        });
        assert_eq!(m.render_path(), RenderPath::MeshRaster);

        let flat = Payload::Screen(Screen::flat(0, 1920, 1080));
        assert_eq!(flat.render_path(), RenderPath::FlatImage);

        let pano = Payload::Screen(Screen {
            projection: ScreenProjection::Equirect {
                horizontal_fov: 360.0,
            },
            ..Screen::flat(0, 8192, 4096)
        });
        assert_eq!(pano.render_path(), RenderPath::Panorama);
    }

    #[test]
    fn over_under_stereo_reports_the_aspect_of_one_eye_not_of_the_packed_frame() {
        // A VR180 frame is 4096x4096 holding two 4096x2048 eyes. Each eye covers half the
        // sphere, so the aspect that matters is 2:1, not the 1:1 the file dimensions suggest.
        let s = Screen {
            stereo: StereoLayout::OverUnder,
            ..Screen::flat(0, 4096, 4096)
        };
        assert_eq!(s.aspect(), 2.0);
        assert_eq!(s.stereo.left_eye_uv(), ([1.0, 0.5], [0.0, 0.0]));
    }

    #[test]
    fn a_flat_screen_is_a_unit_height_rectangle_of_the_right_aspect() {
        let s = Screen::flat(0, 1600, 800);
        let b = s.bounds();
        assert_eq!(b.extent()[0], 2.0);
        assert_eq!(b.extent()[1], 1.0);
        assert!(!s.is_panorama());
    }
}
