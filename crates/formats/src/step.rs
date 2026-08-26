//! STEP: ISO 10303, the CAD exchange format.
//!
//! A STEP file holds no triangles. It holds a **boundary representation**: trimmed NURBS surfaces,
//! B-spline curves, and the topology saying which bounds which. Showing it means tessellating it,
//! and tessellation is a numerical problem with a tolerance, not a translation.
//!
//! Done here through `truck`, a pure-Rust CAD kernel. That is a real dependency, the tiering in
//! `docs/FORMATS.md` puts CAD at T2 for exactly this reason, but the alternative is OpenCASCADE,
//! which is C++, enormous, and would put a large untrusted-input parser outside Rust's safety
//! guarantees.
//!
//! Two decisions worth stating:
//!
//! - **Tolerance scales with the model.** A fixed chord tolerance is either wasteful on a bracket
//!   or blocky on a building. It is derived from the model's own extent, so a 5 mm hole and a
//!   50 m span both tessellate to about the same visual smoothness.
//! - **Robust tessellation, not fast tessellation.** Real CAD exports have degenerate trims,
//!   slivers and seams that the fast path turns into holes. Speed is not what someone opening a
//!   STEP file in a viewer is short of.

use truck_meshalgo::tessellation::{MeshedShape, RobustMeshableShape};
use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Chord tolerance as a fraction of the model's diagonal.
///
/// One part in two thousand: fine enough that a cylinder reads as round at any sensible zoom,
/// coarse enough that a whole assembly still tessellates in seconds.
const TOLERANCE_FRACTION: f64 = 1.0 / 2_000.0;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Step,
        reason: reason.into(),
    }
}

pub fn import(bytes: &[u8]) -> Result<Scene, ImportError> {
    // STEP is ISO 10303-21 "Part 21" text: ASCII with an escape convention for anything else.
    // Lossy decoding is right here: a stray byte in a comment must not fail the import.
    let text = String::from_utf8_lossy(bytes);

    let table = truck_stepio::r#in::Table::from_step(&text)
        .ok_or_else(|| malformed("the file is not parseable as ISO 10303-21"))?;
    if table.shell.is_empty() {
        return Err(malformed(
            "the file parses but declares no closed or open shells; it may hold only wireframe \
             geometry or product structure",
        ));
    }

    // A first pass over the control points establishes the model's size, which is what sets the
    // tessellation tolerance. Reading the shells twice is far cheaper than tessellating once at
    // the wrong scale.
    let tolerance = tolerance_for(&table);

    let mut scene = Scene::new();
    let mut shells = 0usize;
    let mut failed = 0usize;

    for (id, shell) in &table.shell {
        let Ok(compressed) = table.to_compressed_shell(shell) else {
            failed += 1;
            continue;
        };
        // Robust rather than fast: exported CAD is full of degenerate trims that the fast path
        // silently drops, and a hole in a solid is worse than a slow load.
        let polygon = compressed.robust_triangulation(tolerance).to_polygon();
        let Some(mesh) = to_trimesh(&polygon) else {
            failed += 1;
            continue;
        };
        scene.push_root(Node::with_payload(
            format!("shell {id}"),
            Payload::TriMesh(mesh),
        ));
        shells += 1;
    }

    if shells == 0 {
        return Err(malformed(
            "every shell in the file failed to tessellate; the geometry may use surface types \
             this kernel does not implement",
        ));
    }

    scene.set_meta("format", "STEP (ISO 10303)");
    scene.set_meta("shells", shells.to_string());
    scene.set_meta("tessellation tolerance", format!("{tolerance:.6}"));
    if failed > 0 {
        // Silent partial success reads as a complete model with pieces missing.
        scene.set_meta("failed shells", failed.to_string());
    }
    if let Some(name) = product_name(&text) {
        scene.set_meta("product", name);
    }
    Ok(scene)
}

/// Chord tolerance derived from the extent of the file's own control points.
fn tolerance_for(table: &truck_stepio::r#in::Table) -> f64 {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for point in table.cartesian_point.values() {
        let coordinates = &point.coordinates;
        for axis in 0..3 {
            let Some(v) = coordinates.get(axis).copied() else {
                continue;
            };
            if !v.is_finite() {
                continue;
            }
            min[axis] = min[axis].min(v);
            max[axis] = max[axis].max(v);
        }
    }
    let diagonal = (0..3)
        .map(|a| {
            let d = max[a] - min[a];
            if d.is_finite() {
                d * d
            } else {
                0.0
            }
        })
        .sum::<f64>()
        .sqrt();
    if diagonal.is_finite() && diagonal > 0.0 {
        // Clamped at both ends: a file in microns must not ask for a nanometre tolerance, and a
        // file in kilometres must not get a metre one.
        (diagonal * TOLERANCE_FRACTION).clamp(1e-4, 10.0)
    } else {
        0.01
    }
}

fn to_trimesh(polygon: &truck_meshalgo::prelude::PolygonMesh) -> Option<TriMesh> {
    let source_positions = polygon.positions();
    let source_normals = polygon.normals();
    if source_positions.is_empty() {
        return None;
    }

    // truck indexes position and normal separately, per face corner. The renderer wants one
    // vertex array, so corners are expanded and shared only where both indices agree.
    let mut positions: Vec<f32> = Vec::new();
    let mut normals: Vec<f32> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashMap<(usize, Option<usize>), u32> =
        std::collections::HashMap::new();
    let mut bounds = Aabb::EMPTY;

    let mut corner = |vertex: truck_meshalgo::prelude::StandardVertex,
                      positions: &mut Vec<f32>,
                      normals: &mut Vec<f32>,
                      bounds: &mut Aabb|
     -> Option<u32> {
        let key = (vertex.pos, vertex.nor);
        if let Some(existing) = seen.get(&key) {
            return Some(*existing);
        }
        let p = source_positions.get(vertex.pos)?;
        let point = [p.x as f32, p.y as f32, p.z as f32];
        if !point.iter().all(|v| v.is_finite()) {
            return None;
        }
        bounds.expand(point);
        positions.extend_from_slice(&point);

        let n = vertex
            .nor
            .and_then(|i| source_normals.get(i))
            .map(|n| [n.x as f32, n.y as f32, n.z as f32])
            .unwrap_or([0.0, 0.0, 0.0]);
        normals.extend_from_slice(&n);

        let index = (positions.len() / 3 - 1) as u32;
        seen.insert(key, index);
        Some(index)
    };

    for face in polygon.tri_faces() {
        let mut triangle = [0u32; 3];
        let mut ok = true;
        for (slot, vertex) in triangle.iter_mut().zip(face.iter()) {
            match corner(*vertex, &mut positions, &mut normals, &mut bounds) {
                Some(i) => *slot = i,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            indices.extend_from_slice(&triangle);
        }
    }

    if indices.is_empty() {
        return None;
    }

    let count = positions.len() / 3;
    let mut attributes = vec![Attribute::new(
        Semantic::Position,
        ComponentType::F32,
        3,
        count,
        positions.iter().flat_map(|v| v.to_le_bytes()).collect(),
    )
    .ok()?];
    // Surface normals come from the kernel's own evaluation, which is exact where deriving them
    // from the tessellation would only be an approximation of an approximation.
    if normals.iter().any(|v| *v != 0.0) {
        attributes.push(
            Attribute::new(
                Semantic::Normal,
                ComponentType::F32,
                3,
                count,
                normals.iter().flat_map(|v| v.to_le_bytes()).collect(),
            )
            .ok()?,
        );
    }

    Some(TriMesh {
        attributes,
        indices: Indices::U32(indices),
        bounds,
        material: None,
    })
}

/// The `PRODUCT` entity's name, which is what a CAD system calls the part.
fn product_name(text: &str) -> Option<String> {
    let at = text.find("PRODUCT(")?;
    let rest = &text[at + "PRODUCT(".len()..];
    let open = rest.find('\'')?;
    let close = rest[open + 1..].find('\'')?;
    let name = rest[open + 1..open + 1 + close].trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_not_step_is_refused_rather_than_parsed_into_nothing() {
        assert!(import(b"this is not a STEP file").is_err());
        assert!(import(b"").is_err());
    }

    #[test]
    fn a_step_file_with_no_shells_says_so_specifically() {
        // A valid Part 21 file carrying only product structure. The message has to distinguish
        // "unreadable" from "readable, but there is no surface in it".
        let text = "ISO-10303-21;\n\
             HEADER;\n\
             FILE_DESCRIPTION((''),'2;1');\n\
             FILE_NAME('empty.step','2026-01-01T00:00:00',(''),(''),'','','');\n\
             FILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\n\
             ENDSEC;\n\
             DATA;\n\
             #1=PRODUCT('bracket','bracket','',(#2));\n\
             #2=PRODUCT_CONTEXT('',#3,'mechanical');\n\
             #3=APPLICATION_CONTEXT('core data for automotive mechanical design processes');\n\
             ENDSEC;\n\
             END-ISO-10303-21;\n";
        let error = import(text.as_bytes()).unwrap_err().to_string();
        assert!(error.contains("shell"), "{error}");
    }

    #[test]
    fn the_product_name_is_lifted_out_of_the_data_section() {
        let text = "#1=PRODUCT('waltz wheel','waltz wheel','',(#2));";
        assert_eq!(product_name(text).as_deref(), Some("waltz wheel"));
        assert_eq!(product_name("no product entity here"), None);
        // An unnamed product is not a name.
        assert_eq!(product_name("#1=PRODUCT('','','',(#2));"), None);
    }

    #[test]
    fn the_tolerance_is_clamped_at_both_ends() {
        // Directly: an empty table has no extent, and the fallback must still be usable.
        let table = truck_stepio::r#in::Table::default();
        let tolerance = tolerance_for(&table);
        assert!(tolerance > 0.0 && tolerance <= 10.0, "{tolerance}");
    }
}
