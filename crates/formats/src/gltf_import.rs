//! glTF 2.0 and 2.1 importer, binary and JSON.
//!
//! Node transforms are flattened into world space on import. glTF hierarchies are frequently deep
//! and the renderer has no transform pass yet, so a child mesh under a scaled parent would
//! otherwise draw at the wrong size.
//!
//! `EXT_meshopt_compression` and `KHR_draco_mesh_compression` are not decoded here. A file using
//! either reports which extension it needs rather than rendering a partial mesh, because a
//! silently empty viewport is worse than a named limitation.

use gltf::mesh::Mode;
use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

fn malformed(binary: bool, reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: if binary {
            FormatId::Glb {
                sixty_four_bit: false,
            }
        } else {
            FormatId::GltfJson
        },
        reason: reason.into(),
    }
}

/// Extensions that change how buffer data is encoded. Without a decoder for these, attribute reads
/// return nothing useful.
const BLOCKING_EXTENSIONS: [&str; 2] = ["KHR_draco_mesh_compression", "EXT_meshopt_compression"];

pub fn import(bytes: &[u8], binary: bool) -> Result<Scene, ImportError> {
    let (document, buffers, _images) =
        gltf::import_slice(bytes).map_err(|e| malformed(binary, e.to_string()))?;

    for ext in document.extensions_required() {
        if BLOCKING_EXTENSIONS.contains(&ext) {
            return Err(malformed(
                binary,
                format!("the file requires {ext}, which has no decoder yet"),
            ));
        }
    }

    let mut scene = Scene::new();
    scene.set_meta("format", if binary { "GLB" } else { "glTF" });
    if let Some(v) = document.default_scene().and_then(|s| s.name()) {
        scene.set_meta("scene", v);
    }

    let roots: Vec<gltf::Node> = match document.default_scene() {
        Some(s) => s.nodes().collect(),
        None => document.nodes().collect(),
    };

    let mut meshes = 0usize;
    for node in roots {
        walk(&node, IDENTITY, &buffers, &mut scene, &mut meshes, binary)?;
    }

    if meshes == 0 {
        return Err(malformed(binary, "no triangle primitives"));
    }
    Ok(scene)
}

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Column-major multiply, matching glTF's own matrix convention.
fn mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for c in 0..4 {
        for r in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k][r] * b[c][k];
            }
            out[c][r] = sum;
        }
    }
    out
}

fn transform_point(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// Directions ignore translation. This is only correct for uniform scale; a non-uniformly scaled
/// node needs the inverse transpose, which is not worth carrying until a file actually needs it.
fn transform_dir(m: &[[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
    let out = [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ];
    let len = (out[0] * out[0] + out[1] * out[1] + out[2] * out[2]).sqrt();
    if len > 1e-8 {
        [out[0] / len, out[1] / len, out[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    }
}

fn walk(
    node: &gltf::Node,
    parent: [[f32; 4]; 4],
    buffers: &[gltf::buffer::Data],
    scene: &mut Scene,
    meshes: &mut usize,
    binary: bool,
) -> Result<(), ImportError> {
    let world = mul(parent, node.transform().matrix());

    if let Some(mesh) = node.mesh() {
        for primitive in mesh.primitives() {
            // Points and lines are valid glTF but have no renderer path here yet.
            if primitive.mode() != Mode::Triangles {
                continue;
            }
            if let Some(payload) = primitive_to_mesh(&primitive, buffers, &world, binary)? {
                let name = node
                    .name()
                    .or_else(|| mesh.name())
                    .unwrap_or("mesh")
                    .to_string();
                scene.push_root(Node::with_payload(name, payload));
                *meshes += 1;
            }
        }
    }

    for child in node.children() {
        walk(&child, world, buffers, scene, meshes, binary)?;
    }
    Ok(())
}

fn primitive_to_mesh(
    primitive: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    world: &[[f32; 4]; 4],
    binary: bool,
) -> Result<Option<Payload>, ImportError> {
    let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));

    let Some(positions) = reader.read_positions() else {
        return Ok(None);
    };
    let mut bounds = Aabb::EMPTY;
    let mut flat_pos: Vec<f32> = Vec::new();
    for p in positions {
        let w = transform_point(world, p);
        bounds.expand(w);
        flat_pos.extend_from_slice(&w);
    }
    let count = flat_pos.len() / 3;
    if count == 0 {
        return Ok(None);
    }

    let mut attributes = vec![f32_attr(Semantic::Position, 3, flat_pos, binary)?];

    if let Some(normals) = reader.read_normals() {
        let flat: Vec<f32> = normals.flat_map(|n| transform_dir(world, n)).collect();
        if flat.len() / 3 == count {
            attributes.push(f32_attr(Semantic::Normal, 3, flat, binary)?);
        }
    }

    if let Some(uv) = reader.read_tex_coords(0) {
        let flat: Vec<f32> = uv.into_f32().flatten().collect();
        if flat.len() / 2 == count {
            attributes.push(f32_attr(Semantic::TexCoord(0), 2, flat, binary)?);
        }
    }

    // Vertex colours if present, otherwise the material base colour so distinct parts stay
    // distinguishable without a texture pipeline.
    let colours: Vec<f32> = match reader.read_colors(0) {
        Some(c) => c.into_rgba_f32().flatten().collect(),
        None => {
            let base = primitive
                .material()
                .pbr_metallic_roughness()
                .base_color_factor();
            base.iter().copied().cycle().take(count * 4).collect()
        }
    };
    if colours.len() / 4 == count {
        attributes.push(f32_attr(Semantic::Color(0), 4, colours, binary)?);
    }

    let indices = match reader.read_indices() {
        Some(i) => {
            let raw: Vec<u32> = i.into_u32().collect();
            for &v in &raw {
                if v as usize >= count {
                    return Err(malformed(
                        binary,
                        format!("index {v} exceeds the {count} vertices in this primitive"),
                    ));
                }
            }
            if count <= u16::MAX as usize {
                Indices::U16(raw.into_iter().map(|v| v as u16).collect())
            } else {
                Indices::U32(raw)
            }
        }
        None => Indices::None,
    };

    Ok(Some(Payload::TriMesh(TriMesh {
        attributes,
        indices,
        bounds,
        material: primitive.material().index().map(|i| i as u32),
    })))
}

fn f32_attr(
    semantic: Semantic,
    components: u8,
    data: Vec<f32>,
    binary: bool,
) -> Result<Attribute, ImportError> {
    let count = data.len() / components as usize;
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Attribute::new(semantic, ComponentType::F32, components, count, bytes)
        .map_err(|e| malformed(binary, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_major_multiply_matches_the_identity() {
        let m = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 3.0, 0.0, 0.0],
            [0.0, 0.0, 4.0, 0.0],
            [5.0, 6.0, 7.0, 1.0],
        ];
        assert_eq!(mul(IDENTITY, m), m);
        assert_eq!(mul(m, IDENTITY), m);
    }

    #[test]
    fn a_point_picks_up_scale_and_translation() {
        let m = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 2.0, 0.0, 0.0],
            [0.0, 0.0, 2.0, 0.0],
            [10.0, 0.0, 0.0, 1.0],
        ];
        assert_eq!(transform_point(&m, [1.0, 1.0, 1.0]), [12.0, 2.0, 2.0]);
    }

    #[test]
    fn a_direction_ignores_translation_and_stays_unit_length() {
        let m = [
            [3.0, 0.0, 0.0, 0.0],
            [0.0, 3.0, 0.0, 0.0],
            [0.0, 0.0, 3.0, 0.0],
            [99.0, 99.0, 99.0, 1.0],
        ];
        let d = transform_dir(&m, [0.0, 1.0, 0.0]);
        assert_eq!(d, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn a_zero_direction_does_not_produce_nan() {
        let d = transform_dir(&IDENTITY, [0.0, 0.0, 0.0]);
        assert!(d.iter().all(|c| c.is_finite()));
    }

    #[test]
    fn nested_transforms_compose() {
        let parent = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [10.0, 0.0, 0.0, 1.0],
        ];
        let child = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 5.0, 0.0, 1.0],
        ];
        let world = mul(parent, child);
        assert_eq!(transform_point(&world, [0.0, 0.0, 0.0]), [10.0, 5.0, 0.0]);
    }

    #[test]
    fn garbage_input_is_rejected_rather_than_panicking() {
        assert!(import(&[0u8; 64], true).is_err());
        assert!(import(b"not json", false).is_err());
    }
}
