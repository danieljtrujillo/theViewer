//! The scene model every format imports into.
//!
//! The shape here is deliberately **glTF 2.1 + `KHR_gaussian_splatting` semantics** rather than a
//! neutral model of our own design (see `docs/ARCHITECTURE.md`). Two things follow from that, and
//! they are the reason this crate looks the way it does:
//!
//! 1. Attributes are **accessor-shaped** — a byte buffer plus a component type plus a normalized
//!    flag — so data stays in whatever precision it arrived in. A `u16`-quantized position array
//!    travels from file to GPU buffer without ever widening to `f32` on the CPU. That is not spec
//!    pedantry; it is the memory posture the whole app is built on.
//! 2. Splat attributes are named and typed exactly as the Khronos extension names and types them,
//!    so importing `.spz` / `.sog` / `.ply` and exporting glTF are both lossless.

#![forbid(unsafe_code)]

pub mod attr;
pub mod camera;
pub mod material;
pub mod payload;

pub use attr::{Attribute, ComponentType, Semantic};
pub use camera::CameraRig;
pub use material::{AlphaMode, Image, Material};
pub use payload::{
    MeshFrame, Payload, PointCloud, Screen, ScreenProjection, SplatCloud, StereoLayout, TriMesh,
    VolumeGrid,
};

/// Axis-aligned bounding box. Doubles as the dequantization range for quantized attributes,
/// which is why it lives at the root rather than inside a geometry type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    /// An intentionally inverted box, so that `expand` on the first point produces a correct
    /// result rather than a box anchored at the origin.
    pub const EMPTY: Aabb = Aabb {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    pub fn expand(&mut self, p: [f32; 3]) {
        for ((lo, hi), v) in self.min.iter_mut().zip(self.max.iter_mut()).zip(p) {
            if v < *lo {
                *lo = v;
            }
            if v > *hi {
                *hi = v;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        (0..3).any(|i| self.min[i] > self.max[i])
    }

    pub fn extent(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    pub fn center(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        let mut c = [0.0f32; 3];
        for (slot, (lo, hi)) in c.iter_mut().zip(self.min.iter().zip(self.max.iter())) {
            *slot = (lo + hi) * 0.5;
        }
        c
    }

    /// Longest axis. The camera framing code wants this, and so does any LOD heuristic.
    pub fn radius(&self) -> f32 {
        let e = self.extent();
        (e[0] * e[0] + e[1] * e[1] + e[2] * e[2]).sqrt() * 0.5
    }

    pub fn union(&self, other: &Aabb) -> Aabb {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let mut r = *self;
        r.expand(other.min);
        r.expand(other.max);
        r
    }
}

impl Default for Aabb {
    fn default() -> Self {
        Aabb::EMPTY
    }
}

/// Column-major 4x4, matching glTF's own convention so node transforms need no transposition on
/// import or export.
pub type Mat4 = [f32; 16];

pub const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// Index into [`Scene::nodes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub transform: Mat4,
    pub children: Vec<NodeId>,
    /// `None` for pure grouping nodes — glTF's model, and it keeps hierarchy intact for formats
    /// that carry meaningful structure (USD, IFC, 3D Tiles) instead of flattening it away.
    pub payload: Option<Payload>,
}

impl Node {
    pub fn group(name: impl Into<String>) -> Self {
        Node {
            name: name.into(),
            transform: IDENTITY,
            children: Vec::new(),
            payload: None,
        }
    }

    pub fn with_payload(name: impl Into<String>, payload: Payload) -> Self {
        Node {
            name: name.into(),
            transform: IDENTITY,
            children: Vec::new(),
            payload: Some(payload),
        }
    }
}

/// A time-varying scene: volumetric video, animated splats, mesh-sequence folders.
///
/// The frame *store* is not here on purpose. `.ares` GOPs, per-frame folders and UVOL manifests
/// all resolve frames lazily through the streaming layer, so a `Sequence` describes the timeline
/// and lets the player pull frames on demand rather than holding 4,911 of them resident.
#[derive(Debug, Clone)]
pub struct Timeline {
    pub frame_count: u32,
    pub fps: f32,
    pub duration_us: u64,
    /// True when frames can be seeked independently — an `.ares` GOP index, a video keyframe
    /// table, a folder of discrete files. False for formats that must be played forward.
    pub seekable: bool,
}

impl Timeline {
    pub fn frame_at(&self, t_seconds: f64) -> u32 {
        if self.fps <= 0.0 || self.frame_count == 0 {
            return 0;
        }
        let f = (t_seconds * self.fps as f64).floor();
        if f < 0.0 {
            return 0;
        }
        (f as u64).min(self.frame_count as u64 - 1) as u32
    }
}

/// Free-form provenance carried from the source file: title, author, capture rig, colour space,
/// licence. Deliberately untyped — every format has metadata we do not want to model, and
/// throwing it away makes the inspector useless.
pub type Metadata = Vec<(String, String)>;

#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub nodes: Vec<Node>,
    pub roots: Vec<NodeId>,
    pub cameras: Vec<CameraRig>,
    pub timeline: Option<Timeline>,
    pub meta: Metadata,
    /// Referenced by [`payload::TriMesh::material`].
    pub materials: Vec<Material>,
    /// Referenced by [`Material::base_color_texture`] and by [`payload::Screen::image`].
    pub images: Vec<Image>,
    /// Image slots the player rewrites as playback advances. The renderer allocates these as
    /// writable textures rather than upload-once ones, so a video frame does not rebuild a bind
    /// group sixty times a second.
    pub dynamic_images: Vec<u32>,
    /// Nodes whose mesh geometry is replaced per frame — volumetric video, mesh sequences.
    pub dynamic_meshes: Vec<NodeId>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(node);
        id
    }

    /// Push a node and register it as a root in one step — the common case for a flat import.
    pub fn push_root(&mut self, node: Node) -> NodeId {
        let id = self.push(node);
        self.roots.push(id);
        id
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize)
    }

    pub fn push_material(&mut self, material: Material) -> u32 {
        self.materials.push(material);
        (self.materials.len() - 1) as u32
    }

    pub fn push_image(&mut self, image: Image) -> u32 {
        self.images.push(image);
        (self.images.len() - 1) as u32
    }

    pub fn material(&self, index: Option<u32>) -> Option<&Material> {
        self.materials.get(index? as usize)
    }

    /// Push an image the player will overwrite each frame. The initial contents are the poster
    /// frame, so a video that has not started decoding yet still shows something.
    pub fn push_dynamic_image(&mut self, image: Image) -> u32 {
        let index = self.push_image(image);
        self.dynamic_images.push(index);
        index
    }

    pub fn is_dynamic_image(&self, index: u32) -> bool {
        self.dynamic_images.contains(&index)
    }

    /// The screen payload, if this scene is one. Videos and pictures produce exactly one; a
    /// geometry scene produces none, which is how the app decides which view mode to enter.
    pub fn screen(&self) -> Option<&payload::Screen> {
        self.nodes.iter().find_map(|n| match &n.payload {
            Some(Payload::Screen(s)) => Some(s),
            _ => None,
        })
    }

    pub fn set_meta(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        match self.meta.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.meta.push((key, value)),
        }
    }

    /// Union of every payload's local bounds. Ignores node transforms for now — correct for the
    /// flat imports that dominate, and the hierarchy-aware version belongs with the renderer's
    /// transform pass rather than here.
    pub fn bounds(&self) -> Aabb {
        self.nodes
            .iter()
            .filter_map(|n| n.payload.as_ref())
            .fold(Aabb::EMPTY, |acc, p| acc.union(&p.bounds()))
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.iter().all(|n| n.payload.is_none())
    }

    /// What the renderer needs to know to pick a path, and what the UI shows in the status bar.
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for node in &self.nodes {
            match &node.payload {
                Some(Payload::TriMesh(m)) => {
                    s.meshes += 1;
                    s.triangles += m.triangle_count();
                    s.vertices += m.vertex_count();
                }
                Some(Payload::PointCloud(p)) => {
                    s.point_clouds += 1;
                    s.points += p.count;
                }
                Some(Payload::SplatCloud(c)) => {
                    s.splat_clouds += 1;
                    s.splats += c.count;
                }
                Some(Payload::VolumeGrid(v)) => {
                    s.volumes += 1;
                    s.voxels += v.voxel_count();
                }
                Some(Payload::Screen(sc)) => {
                    s.screens += 1;
                    s.pixels += sc.width as usize * sc.height as usize;
                }
                None => {}
            }
        }
        s
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub meshes: usize,
    pub triangles: usize,
    pub vertices: usize,
    pub point_clouds: usize,
    pub points: usize,
    pub splat_clouds: usize,
    pub splats: usize,
    pub volumes: usize,
    pub voxels: usize,
    pub screens: usize,
    pub pixels: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_aabb_expands_to_the_first_point_not_the_origin() {
        let mut b = Aabb::EMPTY;
        assert!(b.is_empty());
        b.expand([5.0, 5.0, 5.0]);
        assert_eq!(b.min, [5.0, 5.0, 5.0]);
        assert_eq!(b.max, [5.0, 5.0, 5.0]);
        assert!(!b.is_empty());
    }

    #[test]
    fn union_ignores_empty_operands() {
        let mut a = Aabb::EMPTY;
        a.expand([0.0, 0.0, 0.0]);
        a.expand([1.0, 2.0, 3.0]);
        assert_eq!(a.union(&Aabb::EMPTY), a);
        assert_eq!(Aabb::EMPTY.union(&a), a);
    }

    #[test]
    fn frame_at_clamps_to_the_last_frame() {
        let t = Timeline {
            frame_count: 272,
            fps: 30.0,
            duration_us: 9_066_667,
            seekable: true,
        };
        assert_eq!(t.frame_at(0.0), 0);
        assert_eq!(t.frame_at(1.0), 30);
        assert_eq!(t.frame_at(1_000.0), 271);
        assert_eq!(t.frame_at(-5.0), 0);
    }

    #[test]
    fn set_meta_overwrites_rather_than_duplicating() {
        let mut s = Scene::new();
        s.set_meta("title", "first");
        s.set_meta("title", "second");
        assert_eq!(s.meta.len(), 1);
        assert_eq!(s.meta[0].1, "second");
    }
}
