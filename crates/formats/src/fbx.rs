//! FBX importer, built on ufbx.
//!
//! ufbx rather than the Autodesk SDK: it is MIT, it is a single C file, and it reads 6.x through
//! 7.7 in both binary and ASCII. It also normalizes the parts of FBX that vary by exporter, which
//! is most of them. FBX is the format Microsoft disabled in 3D Viewer over a parser CVE, so it is
//! the one that most deserves a hardened reader.
//!
//! Two FBX quirks are handled here rather than left to the renderer:
//!
//! - **Units.** FBX files carry a unit scale and Maya writes centimetres by default, so a
//!   character exported from Maya arrives a hundred times larger than the same character from
//!   Blender. ufbx normalizes to metres when asked, which makes framing behave.
//! - **Geometry transforms.** FBX allows a transform on the geometry that is not inherited by
//!   child nodes. `geometry_to_world` already folds that in; `node_to_world` does not.
//!
//! Textures come from three places, tried in order: content embedded in the file, the absolute
//! path the exporter recorded, and the file name resolved against the model's own directory. The
//! third is what actually works for files that have moved between machines.
//!
//! ufbx hands out `Ref<T>` smart pointers. Borrowing one to reach the `&T` a helper expects reads
//! to clippy as a needless borrow, but removing the borrow passes the `Ref` by value and does not
//! compile, so the lint is suppressed for this module.
#![allow(clippy::needless_borrow)]

use std::collections::HashMap;
use std::path::Path;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, AlphaMode, Material, Node, Payload, Scene};

use crate::{sidecar, ImportError};



fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Fbx,
        reason: reason.into(),
    }
}

pub fn import(bytes: &[u8], base: Option<&Path>) -> Result<Scene, ImportError> {
    let opts = ufbx::LoadOpts {
        // Fold geometry transforms into the vertices so the renderer needs no special case.
        geometry_transform_handling: ufbx::GeometryTransformHandling::ModifyGeometry,
        // Normalize to metres and to a right-handed Y-up basis, matching glTF and the scene model.
        target_unit_meters: 1.0,
        target_axes: ufbx::CoordinateAxes::right_handed_y_up(),
        generate_missing_normals: true,
        // The file is already in memory and may reference paths from another machine. Reading them
        // would be both useless and a filesystem reach from untrusted content.
        load_external_files: false,
        ignore_missing_external_files: true,
        ..Default::default()
    };

    let root = ufbx::load_memory(bytes, opts)
        .map_err(|e| malformed(format!("{:?}: {}", e.type_, e.description)))?;
    // SceneRoot derefs to Scene and owns the underlying allocation.
    let fbx = &*root;

    let mut scene = Scene::new();
    scene.set_meta("format", "FBX");
    if !fbx.metadata.creator.is_empty() {
        scene.set_meta("creator", fbx.metadata.creator.to_string());
    }
    scene.set_meta("fbx_version", fbx.metadata.version.to_string());

    // Materials first, so meshes can reference them by index.
    let mut image_cache: HashMap<String, u32> = HashMap::new();
    let mut material_index: HashMap<usize, u32> = HashMap::new();
    for (i, m) in fbx.materials.iter().enumerate() {
        let material = convert_material(&m, base, &mut scene, &mut image_cache);
        let index = scene.push_material(material);
        material_index.insert(i, index);
    }

    let mut emitted = 0usize;
    for node in fbx.nodes.iter() {
        let Some(mesh) = node.mesh.as_ref() else {
            continue;
        };
        if node.is_root {
            continue;
        }
        let name = if node.element.name.is_empty() {
            "mesh".to_string()
        } else {
            node.element.name.to_string()
        };
        emitted += convert_mesh(&node, &mesh, &name, &material_index, &mut scene)?;
    }

    if emitted == 0 {
        return Err(malformed("no triangle geometry"));
    }
    Ok(scene)
}

/// One node's mesh, split into one payload per material.
///
/// `material_parts` already groups faces by material, which avoids a pass over `face_material` and
/// gives the exact triangle count per part up front.
fn convert_mesh(
    node: &ufbx::Node,
    mesh: &ufbx::Mesh,
    name: &str,
    material_index: &HashMap<usize, u32>,
    scene: &mut Scene,
) -> Result<usize, ImportError> {
    let m = &node.geometry_to_world;
    let has_uv = mesh.vertex_uv.exists;
    let has_normal = mesh.vertex_normal.exists;
    let has_color = mesh.vertex_color.exists;

    let mut emitted = 0usize;
    // Space for the largest polygon in the mesh, so triangulation never reallocates per face.
    let mut tri = vec![0u32; (mesh.max_face_triangles.max(1)) * 3];

    for (part_index, part) in mesh.material_parts.iter().enumerate() {
        if part.num_triangles == 0 {
            continue;
        }

        // One output vertex per distinct source index, so a shared corner stays shared.
        let mut remap: HashMap<u32, u32> = HashMap::new();
        let mut pos: Vec<f32> = Vec::new();
        let mut nrm: Vec<f32> = Vec::new();
        let mut uv: Vec<f32> = Vec::new();
        let mut col: Vec<f32> = Vec::new();
        let mut indices: Vec<u32> = Vec::with_capacity(part.num_triangles * 3);
        let mut bounds = Aabb::EMPTY;

        for &face_index in part.face_indices.as_ref() {
            let face = mesh.faces[face_index as usize];
            let count = ufbx::triangulate_face(&mut tri, mesh, face);
            for &index in &tri[..(count as usize) * 3] {
                let next = remap.len() as u32;
                let local = *remap.entry(index).or_insert_with(|| {
                    let p = transform_point(m, mesh.vertex_position[index as usize]);
                    bounds.expand(p);
                    pos.extend_from_slice(&p);
                    if has_normal {
                        let n = transform_dir(m, mesh.vertex_normal[index as usize]);
                        nrm.extend_from_slice(&n);
                    }
                    if has_uv {
                        let t = mesh.vertex_uv[index as usize];
                        // FBX texture space is bottom-up, like OBJ.
                        uv.extend_from_slice(&[t.x as f32, 1.0 - t.y as f32]);
                    }
                    if has_color {
                        let c = mesh.vertex_color[index as usize];
                        col.extend_from_slice(&[c.x as f32, c.y as f32, c.z as f32, c.w as f32]);
                    }
                    next
                });
                indices.push(local);
            }
        }

        let count = remap.len();
        if count == 0 {
            continue;
        }

        let mut attributes = vec![f32_attr(Semantic::Position, 3, pos)?];
        if has_normal {
            attributes.push(f32_attr(Semantic::Normal, 3, nrm)?);
        }
        if has_uv {
            attributes.push(f32_attr(Semantic::TexCoord(0), 2, uv)?);
        }
        if has_color {
            attributes.push(f32_attr(Semantic::Color(0), 4, col)?);
        }

        let indices = if count <= u16::MAX as usize {
            Indices::U16(indices.into_iter().map(|i| i as u16).collect())
        } else {
            Indices::U32(indices)
        };

        // material_parts is indexed by the mesh's own material slot, which maps into the scene's
        // material list through the element index.
        let material = mesh
            .materials
            .get(part_index)
            .and_then(|mat| material_index.get(&(mat.element.typed_id as usize)))
            .copied();

        let label = if mesh.materials.len() > 1 {
            format!("{name}:{part_index}")
        } else {
            name.to_string()
        };

        scene.push_root(Node::with_payload(
            label,
            Payload::TriMesh(TriMesh {
                attributes,
                indices,
                bounds,
                material,
            }),
        ));
        emitted += 1;
    }
    Ok(emitted)
}

fn convert_material(
    m: &ufbx::Material,
    base: Option<&Path>,
    scene: &mut Scene,
    cache: &mut HashMap<String, u32>,
) -> Material {
    let mut out = Material::named(m.element.name.to_string());

    // Prefer the PBR interpretation ufbx derives; fall back to the legacy FBX maps.
    let pbr = &m.pbr;
    if pbr.base_color.has_value {
        let c = pbr.base_color.value_vec4;
        out.base_color = [c.x as f32, c.y as f32, c.z as f32, out.base_color[3]];
    } else if m.fbx.diffuse_color.has_value {
        let c = m.fbx.diffuse_color.value_vec4;
        out.base_color = [c.x as f32, c.y as f32, c.z as f32, out.base_color[3]];
    }
    if pbr.roughness.has_value {
        out.roughness = (pbr.roughness.value_vec4.x as f32).clamp(0.0, 1.0);
    }
    if pbr.metalness.has_value {
        out.metallic = (pbr.metalness.value_vec4.x as f32).clamp(0.0, 1.0);
    }
    if pbr.emission_color.has_value {
        let c = pbr.emission_color.value_vec4;
        out.emissive = [c.x as f32, c.y as f32, c.z as f32];
    }
    // FBX stores transparency, not opacity.
    if pbr.opacity.has_value {
        let a = (pbr.opacity.value_vec4.x as f32).clamp(0.0, 1.0);
        out.base_color[3] = a;
        if a < 0.999 {
            out.alpha_mode = AlphaMode::Blend;
        }
    }

    let texture = pbr
        .base_color
        .texture
        .as_ref()
        .or(m.fbx.diffuse_color.texture.as_ref());
    if let Some(t) = texture {
        if let Some(index) = load_texture(t, base, scene, cache) {
            out.base_color_texture = Some(index);
            if out.base_color[..3] == [0.0, 0.0, 0.0] {
                out.base_color = [1.0, 1.0, 1.0, out.base_color[3]];
            }
        }
    }
    out
}

/// Embedded content first, then the recorded paths, then the file name against the model folder.
fn load_texture(
    t: &ufbx::Texture,
    base: Option<&Path>,
    scene: &mut Scene,
    cache: &mut HashMap<String, u32>,
) -> Option<u32> {
    let key = if !t.filename.is_empty() {
        t.filename.to_string()
    } else {
        t.element.name.to_string()
    };
    if let Some(index) = cache.get(&key) {
        return Some(*index);
    }

    // Annotated: a newer ufbx exposes more than one AsRef here, and inference stops being able
    // to pick.
    let content: &[u8] = t.content.as_ref();
    let image = if !content.is_empty() {
        sidecar::decode_image(content, &key)
    } else {
        None
    }
    .or_else(|| {
        let recorded = Path::new(t.absolute_filename.as_ref());
        if recorded.is_file() {
            sidecar::load_image(recorded)
        } else {
            None
        }
    })
    .or_else(|| {
        let base = base?;
        let reference = if !t.relative_filename.is_empty() {
            t.relative_filename.as_ref()
        } else {
            t.filename.as_ref()
        };
        let path = sidecar::resolve_texture(base, reference)?;
        sidecar::load_image(&path)
    })?;

    let index = scene.push_image(image);
    cache.insert(key, index);
    Some(index)
}

fn transform_point(m: &ufbx::Matrix, p: ufbx::Vec3) -> [f32; 3] {
    let v = ufbx::transform_position(m, p);
    [v.x as f32, v.y as f32, v.z as f32]
}

fn transform_dir(m: &ufbx::Matrix, d: ufbx::Vec3) -> [f32; 3] {
    let v = ufbx::transform_direction(m, d);
    let len = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
    if len > 1e-9 {
        [(v.x / len) as f32, (v.y / len) as f32, (v.z / len) as f32]
    } else {
        [0.0, 0.0, 0.0]
    }
}

fn f32_attr(semantic: Semantic, components: u8, data: Vec<f32>) -> Result<Attribute, ImportError> {
    let count = data.len() / components as usize;
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Attribute::new(semantic, ComponentType::F32, components, count, bytes)
        .map_err(|e| malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(import(&[0u8; 128], None).is_err());
        assert!(import(b"", None).is_err());
    }

    #[test]
    fn a_truncated_binary_header_is_rejected() {
        assert!(import(b"Kaydara FBX Binary  \x00", None).is_err());
    }

    #[test]
    fn a_minimal_ascii_file_with_no_geometry_reports_that() {
        let src = b"; FBX 7.3.0 project file\nFBXHeaderExtension:  {\n\tFBXVersion: 7300\n}\n";
        let err = import(src, None);
        assert!(err.is_err(), "a file with no meshes must not import as empty");
    }
}
