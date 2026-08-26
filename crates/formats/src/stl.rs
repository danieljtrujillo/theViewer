//! STL importer, binary and ASCII.
//!
//! STL stores three unshared vertices per triangle and no topology at all, so a cube arrives as 36
//! vertices rather than 8. Welding by exact position bits recovers the shared vertices, which
//! matters because the renderer derives smooth normals from adjacency: without welding, every
//! triangle keeps its own corners and the result shades flat regardless of the true surface.
//!
//! The per-facet normal STL records is discarded. It is frequently wrong or zero in files exported
//! by CAD tools, and a derived normal is both cheaper to trust and consistent with the welded
//! topology.

use std::collections::HashMap;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

fn malformed(binary: bool, reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: if binary {
            FormatId::StlBinary
        } else {
            FormatId::StlAscii
        },
        reason: reason.into(),
    }
}

pub fn import_binary(bytes: &[u8]) -> Result<Scene, ImportError> {
    if bytes.len() < 84 {
        return Err(malformed(true, "file is shorter than the 84-byte header"));
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let needed = 84usize
        .checked_add(count.checked_mul(50).ok_or_else(|| {
            malformed(true, "triangle count overflows the addressable range")
        })?)
        .ok_or_else(|| malformed(true, "triangle count overflows the addressable range"))?;
    if bytes.len() < needed {
        return Err(malformed(
            true,
            format!(
                "{count} triangles need {needed} bytes, the file has {}",
                bytes.len()
            ),
        ));
    }

    let mut tris = Vec::with_capacity(count * 3);
    for t in 0..count {
        // Skip the recorded facet normal; it is unreliable and gets derived instead.
        let base = 84 + t * 50 + 12;
        for v in 0..3 {
            let o = base + v * 12;
            tris.push([
                f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]),
                f32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]),
                f32::from_le_bytes([bytes[o + 8], bytes[o + 9], bytes[o + 10], bytes[o + 11]]),
            ]);
        }
    }
    build(tris, true)
}

pub fn import_ascii(bytes: &[u8]) -> Result<Scene, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed(false, "file is not UTF-8"))?;
    let mut tris: Vec<[f32; 3]> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with("vertex") {
            continue;
        }
        let mut p = line.split_ascii_whitespace();
        p.next();
        let x = p.next().and_then(|t| t.parse().ok());
        let y = p.next().and_then(|t| t.parse().ok());
        let z = p.next().and_then(|t| t.parse().ok());
        match (x, y, z) {
            (Some(x), Some(y), Some(z)) => tris.push([x, y, z]),
            _ => return Err(malformed(false, "malformed vertex line")),
        }
    }
    if tris.len() % 3 != 0 {
        return Err(malformed(
            false,
            format!("{} vertices do not divide into triangles", tris.len()),
        ));
    }
    build(tris, false)
}

fn build(tris: Vec<[f32; 3]>, binary: bool) -> Result<Scene, ImportError> {
    if tris.is_empty() {
        return Err(malformed(binary, "no triangles"));
    }

    // Weld by exact bit pattern. Positions written by the same exporter for a shared corner are
    // bit-identical, so this recovers real adjacency without the tolerance guesswork that a
    // spatial hash would need.
    let mut map: HashMap<[u32; 3], u32> = HashMap::new();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(tris.len());
    let mut bounds = Aabb::EMPTY;

    for v in &tris {
        let key = [v[0].to_bits(), v[1].to_bits(), v[2].to_bits()];
        let index = *map.entry(key).or_insert_with(|| {
            bounds.expand(*v);
            positions.push(*v);
            (positions.len() - 1) as u32
        });
        indices.push(index);
    }

    let flat: Vec<f32> = positions.iter().flat_map(|p| p.iter().copied()).collect();
    let count = positions.len();
    let mut data = Vec::with_capacity(flat.len() * 4);
    for v in &flat {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let position = Attribute::new(Semantic::Position, ComponentType::F32, 3, count, data)
        .map_err(|e| malformed(binary, e.to_string()))?;

    let indices = if count <= u16::MAX as usize {
        Indices::U16(indices.into_iter().map(|i| i as u16).collect())
    } else {
        Indices::U32(indices)
    };

    let mut scene = Scene::new();
    scene.set_meta("format", if binary { "STL (binary)" } else { "STL (ASCII)" });
    scene.push_root(Node::with_payload(
        "stl",
        Payload::TriMesh(TriMesh {
            attributes: vec![position],
            indices,
            bounds,
            material: None,
        }),
    ));
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_stl(triangles: &[[[f32; 3]; 3]]) -> Vec<u8> {
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
        for t in triangles {
            out.extend_from_slice(&[0u8; 12]); // facet normal, ignored on import
            for v in t {
                for c in v {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            out.extend_from_slice(&[0u8; 2]); // attribute byte count
        }
        out
    }

    #[test]
    fn binary_triangle_parses() {
        let src = binary_stl(&[[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]]);
        let scene = import_binary(&src).unwrap();
        assert_eq!(scene.summary().triangles, 1);
        assert_eq!(scene.summary().vertices, 3);
    }

    #[test]
    fn shared_corners_are_welded() {
        // Two triangles sharing an edge: six references, four distinct positions.
        let src = binary_stl(&[
            [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]],
            [[0.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
        ]);
        let scene = import_binary(&src).unwrap();
        assert_eq!(scene.summary().triangles, 2);
        assert_eq!(scene.summary().vertices, 4);
    }

    #[test]
    fn a_truncated_binary_file_is_rejected_before_allocating() {
        let mut src = vec![0u8; 84];
        src[80..84].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(import_binary(&src).is_err());
    }

    #[test]
    fn a_header_only_file_is_rejected() {
        let src = binary_stl(&[]);
        assert!(import_binary(&src).is_err());
    }

    #[test]
    fn ascii_triangle_parses() {
        let src = b"solid test\n\
                    facet normal 0 0 1\n outer loop\n\
                    vertex 0 0 0\n vertex 1 0 0\n vertex 0 1 0\n\
                    endloop\n endfacet\nendsolid test\n";
        let scene = import_ascii(src).unwrap();
        assert_eq!(scene.summary().triangles, 1);
    }

    #[test]
    fn an_ascii_file_with_a_partial_facet_is_rejected() {
        let src = b"solid test\nfacet normal 0 0 1\nouter loop\n\
                    vertex 0 0 0\nvertex 1 0 0\nendloop\nendfacet\n";
        assert!(import_ascii(src).is_err());
    }

    #[test]
    fn bounds_cover_every_welded_vertex() {
        let src = binary_stl(&[[[-2.0, 0.0, 0.0], [5.0, 0.0, 0.0], [0.0, 3.0, -4.0]]]);
        let scene = import_binary(&src).unwrap();
        assert_eq!(scene.bounds().min, [-2.0, 0.0, -4.0]);
        assert_eq!(scene.bounds().max, [5.0, 3.0, 0.0]);
    }
}
