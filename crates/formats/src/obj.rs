//! Wavefront OBJ importer.
//!
//! OBJ keeps a separate index stream per attribute, so `f 1/2/3` names vertex 1, texcoord 2 and
//! normal 3 independently. GPUs need one index per vertex, so each distinct triple becomes one
//! output vertex. The cache below is what stops a 12 k-triangle frame turning into 36 k vertices.
//!
//! Indices are 1-based and may be negative, in which case they count backwards from the end of the
//! list *as it stands at that point in the file*, which is why resolution happens during the face
//! pass rather than afterwards.
//!
//! `mtllib` sidecars load automatically, and `usemtl` splits the file into one mesh per material.
//! Each split gets a compacted vertex array holding only the vertices it references, so a model
//! with twenty materials does not upload the full vertex list twenty times.

use std::collections::HashMap;
use std::path::Path;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::{mtl, ImportError};

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Obj,
        reason: reason.into(),
    }
}

/// Resolve a 1-based or negative OBJ index against the current list length.
fn resolve(raw: i64, len: usize) -> Option<usize> {
    if raw > 0 {
        let i = (raw - 1) as usize;
        (i < len).then_some(i)
    } else if raw < 0 {
        let back = (-raw) as usize;
        (back <= len).then(|| len - back)
    } else {
        None
    }
}

/// One run of faces sharing a material.
struct Group {
    material: Option<u32>,
    name: String,
    indices: Vec<u32>,
}

pub fn import(bytes: &[u8], base: Option<&Path>) -> Result<Scene, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("file is not valid UTF-8"))?;

    let mut scene = Scene::new();
    scene.set_meta("format", "OBJ");

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut texcoords: Vec<[f32; 2]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();

    let mut cache: HashMap<(usize, usize, usize), u32> = HashMap::new();
    let mut out_pos: Vec<[f32; 3]> = Vec::new();
    let mut out_uv: Vec<[f32; 2]> = Vec::new();
    let mut out_nrm: Vec<[f32; 3]> = Vec::new();
    let mut bounds = Aabb::EMPTY;

    let mut saw_uv = false;
    let mut saw_normal = false;

    let mut material_names: HashMap<String, u32> = HashMap::new();
    let mut groups: Vec<Group> = vec![Group {
        material: None,
        name: String::new(),
        indices: Vec::new(),
    }];
    let mut object_name = String::new();

    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_ascii_whitespace();
        let Some(tag) = parts.next() else { continue };
        let rest = line[tag.len()..].trim();

        match tag {
            "v" => {
                let v = read3(&mut parts)
                    .ok_or_else(|| malformed(format!("line {}: malformed v", line_no + 1)))?;
                bounds.expand(v);
                positions.push(v);
            }
            "vt" => {
                let u: f32 = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0.0);
                let v: f32 = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0.0);
                // OBJ texture space is bottom-up; glTF and every GPU convention here is top-down.
                texcoords.push([u, 1.0 - v]);
            }
            "vn" => {
                let n = read3(&mut parts)
                    .ok_or_else(|| malformed(format!("line {}: malformed vn", line_no + 1)))?;
                normals.push(n);
            }
            "mtllib" => {
                if let Some(base) = base {
                    for name in rest.split_whitespace() {
                        let path = base.join(name);
                        if let Ok(text) = std::fs::read_to_string(&path) {
                            let found = mtl::parse(&text, base, &mut scene);
                            material_names.extend(found);
                        }
                    }
                }
            }
            "usemtl" => {
                let material = material_names.get(rest).copied();
                // Start a new group unless the current one is still empty.
                match groups.last_mut() {
                    Some(g) if g.indices.is_empty() => {
                        g.material = material;
                        g.name = rest.to_string();
                    }
                    _ => groups.push(Group {
                        material,
                        name: rest.to_string(),
                        indices: Vec::new(),
                    }),
                }
            }
            "o" | "g" if object_name.is_empty() => {
                object_name = rest.to_string();
            }
            "f" => {
                let mut poly: Vec<u32> = Vec::with_capacity(4);
                for token in parts {
                    let mut fields = token.split('/');
                    let vi = fields
                        .next()
                        .and_then(|t| t.parse::<i64>().ok())
                        .and_then(|r| resolve(r, positions.len()))
                        .ok_or_else(|| {
                            malformed(format!("line {}: bad vertex index", line_no + 1))
                        })?;
                    let ti = fields
                        .next()
                        .filter(|t| !t.is_empty())
                        .and_then(|t| t.parse::<i64>().ok())
                        .and_then(|r| resolve(r, texcoords.len()));
                    let ni = fields
                        .next()
                        .filter(|t| !t.is_empty())
                        .and_then(|t| t.parse::<i64>().ok())
                        .and_then(|r| resolve(r, normals.len()));

                    // usize::MAX stands in for "absent" so the key stays a plain tuple.
                    let key = (vi, ti.unwrap_or(usize::MAX), ni.unwrap_or(usize::MAX));
                    let index = *cache.entry(key).or_insert_with(|| {
                        out_pos.push(positions[vi]);
                        out_uv.push(ti.map(|i| texcoords[i]).unwrap_or([0.0, 0.0]));
                        out_nrm.push(ni.map(|i| normals[i]).unwrap_or([0.0, 0.0, 0.0]));
                        if ti.is_some() {
                            saw_uv = true;
                        }
                        if ni.is_some() {
                            saw_normal = true;
                        }
                        (out_pos.len() - 1) as u32
                    });
                    poly.push(index);
                }
                // Fan triangulation, matching the PLY importer.
                let group = groups.last_mut().expect("at least one group always exists");
                for i in 1..poly.len().saturating_sub(1) {
                    group.indices.extend_from_slice(&[poly[0], poly[i], poly[i + 1]]);
                }
            }
            _ => {}
        }
    }

    if out_pos.is_empty() {
        return Err(malformed("no faces reference any vertices"));
    }

    let total: usize = groups.iter().map(|g| g.indices.len()).sum();
    if total == 0 {
        return Err(malformed("no faces reference any vertices"));
    }

    for group in groups.iter().filter(|g| !g.indices.is_empty()) {
        // Compact so each split uploads only the vertices it uses.
        let mut remap: HashMap<u32, u32> = HashMap::new();
        let mut pos = Vec::new();
        let mut uv = Vec::new();
        let mut nrm = Vec::new();
        let mut indices = Vec::with_capacity(group.indices.len());
        let mut local_bounds = Aabb::EMPTY;

        for &i in &group.indices {
            let next = remap.len() as u32;
            let local = *remap.entry(i).or_insert_with(|| {
                let v = out_pos[i as usize];
                local_bounds.expand(v);
                pos.extend_from_slice(&v);
                uv.extend_from_slice(&out_uv[i as usize]);
                nrm.extend_from_slice(&out_nrm[i as usize]);
                next
            });
            indices.push(local);
        }

        let count = remap.len();
        let mut attributes = vec![f32_attr(Semantic::Position, 3, pos)?];
        if saw_normal {
            attributes.push(f32_attr(Semantic::Normal, 3, nrm)?);
        }
        if saw_uv {
            attributes.push(f32_attr(Semantic::TexCoord(0), 2, uv)?);
        }

        let indices = if count <= u16::MAX as usize {
            Indices::U16(indices.into_iter().map(|i| i as u16).collect())
        } else {
            Indices::U32(indices)
        };

        let name = if !group.name.is_empty() {
            group.name.clone()
        } else if !object_name.is_empty() {
            object_name.clone()
        } else {
            "obj".to_string()
        };

        scene.push_root(Node::with_payload(
            name,
            Payload::TriMesh(TriMesh {
                attributes,
                indices,
                bounds: local_bounds,
                material: group.material,
            }),
        ));
    }

    Ok(scene)
}

fn read3<'a>(parts: &mut impl Iterator<Item = &'a str>) -> Option<[f32; 3]> {
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    Some([x, y, z])
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
    fn a_triangle_with_no_attributes_parses() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let scene = import(src, None).unwrap();
        let s = scene.summary();
        assert_eq!(s.vertices, 3);
        assert_eq!(s.triangles, 1);
    }

    #[test]
    fn shared_vertices_are_reused_rather_than_duplicated() {
        let src = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf 1 3 4\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.summary().vertices, 4);
        assert_eq!(scene.summary().triangles, 2);
    }

    #[test]
    fn distinct_attribute_triples_over_one_position_become_distinct_vertices() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\n\
                    vn 0 0 1\nvn 0 1 0\n\
                    f 1//1 2//1 3//1\nf 1//2 2//2 3//2\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.summary().vertices, 6);
        assert_eq!(scene.summary().triangles, 2);
    }

    #[test]
    fn negative_indices_count_back_from_the_current_end() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.summary().triangles, 1);
        assert_eq!(scene.bounds().max, [1.0, 1.0, 0.0]);
    }

    #[test]
    fn quads_are_triangulated() {
        let src = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n";
        assert_eq!(import(src, None).unwrap().summary().triangles, 2);
    }

    #[test]
    fn texture_v_is_flipped_to_a_top_down_convention() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0.25 0.75\nf 1/1 2/1 3/1\n";
        let scene = import(src, None).unwrap();
        let viewer_scene::Payload::TriMesh(m) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected a mesh");
        };
        let uv = m.attribute(&Semantic::TexCoord(0)).unwrap();
        let mut out = [0.0f32; 2];
        uv.get_f32(0, &mut out);
        assert_eq!(out, [0.25, 0.25]);
    }

    #[test]
    fn usemtl_splits_the_file_into_one_mesh_per_material() {
        let src = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
                    usemtl a\nf 1 2 3\n\
                    usemtl b\nf 1 3 4\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.nodes.len(), 2);
        assert_eq!(scene.nodes[0].name, "a");
        assert_eq!(scene.nodes[1].name, "b");
        assert_eq!(scene.summary().triangles, 2);
    }

    #[test]
    fn each_split_carries_only_the_vertices_it_references() {
        // Two disjoint triangles over six positions; each split must hold three, not six.
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nv 5 0 0\nv 6 0 0\nv 5 1 0\n\
                    usemtl a\nf 1 2 3\n\
                    usemtl b\nf 4 5 6\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.nodes.len(), 2);
        assert_eq!(scene.summary().vertices, 6);
        for node in &scene.nodes {
            let viewer_scene::Payload::TriMesh(m) = node.payload.as_ref().unwrap() else {
                panic!("expected a mesh");
            };
            assert_eq!(m.vertex_count(), 3);
        }
    }

    #[test]
    fn a_second_usemtl_before_any_face_does_not_create_an_empty_mesh() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nusemtl a\nusemtl b\nf 1 2 3\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.nodes.len(), 1);
        assert_eq!(scene.nodes[0].name, "b");
    }

    #[test]
    fn per_split_bounds_cover_only_that_split() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nv 5 0 0\nv 6 0 0\nv 5 1 0\n\
                    usemtl a\nf 1 2 3\n\
                    usemtl b\nf 4 5 6\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.nodes[0].payload.as_ref().unwrap().bounds().max[0], 1.0);
        assert_eq!(scene.nodes[1].payload.as_ref().unwrap().bounds().min[0], 5.0);
        // The scene bound still spans both.
        assert_eq!(scene.bounds().max[0], 6.0);
    }

    #[test]
    fn an_out_of_range_index_is_rejected() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 99\n";
        assert!(import(src, None).is_err());
    }

    #[test]
    fn a_file_without_faces_is_rejected() {
        let src = b"v 0 0 0\nv 1 0 0\nv 0 1 0\n";
        assert!(import(src, None).is_err());
    }

    #[test]
    fn comments_and_unknown_directives_are_skipped() {
        let src = b"# comment\nmtllib scene.mtl\no thing\ns off\n\
                    v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let scene = import(src, None).unwrap();
        assert_eq!(scene.summary().triangles, 1);
        assert_eq!(scene.nodes[0].name, "thing");
    }

    #[test]
    fn a_missing_mtllib_does_not_fail_the_import() {
        let src = b"mtllib nope.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nusemtl x\nf 1 2 3\n";
        let scene = import(src, Some(Path::new("."))).unwrap();
        assert_eq!(scene.summary().triangles, 1);
        assert!(scene.materials.is_empty());
    }
}
