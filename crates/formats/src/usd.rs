//! OpenUSD — `.usda`, `.usdc` and `.usdz`.
//!
//! USD is a *composition* system before it is a file format: a stage is layers stacked in
//! strength order, with references, payloads, variants and inherits resolved into one composed
//! view. Reading the bytes of a `.usda` and looking for `points` gets you a demo, not a viewer —
//! any real asset puts its geometry behind at least a reference and usually a variant set.
//!
//! So this goes through `openusd`, a native Rust implementation of the composition engine, and
//! asks the **composed stage** for its prims. All three encodings arrive through the same door.
//!
//! Three things USD does that the scene model has to be told about:
//!
//! - **`metersPerUnit`.** USD carries its own scale, and assets are authored in centimetres about
//!   as often as in metres.
//! - **`upAxis`.** USD allows Y-up or Z-up and says which. Z-up assets — most of architecture and
//!   all of DCC-exported CAD — arrive lying on their side unless the axis is honoured.
//! - **Polygons, not triangles.** `faceVertexCounts` is per face and quads are the norm, so the
//!   mesh has to be triangulated on the way in.

use std::path::Path;

use openusd::schemas::geom::{Mesh, PointBased, Xform, Xformable};
use openusd::usd::{PrimPredicate, SchemaBase, Stage};
use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Refuse a prim that declares more geometry than any asset plausibly holds, before the
/// declaration becomes an allocation.
const MAX_POINTS: usize = 50_000_000;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Usdc,
        reason: reason.into(),
    }
}

pub fn import(path: &Path) -> Result<Scene, ImportError> {
    let identifier = path.to_string_lossy().replace('\\', "/");
    let stage = Stage::open(&identifier)
        .map_err(|e| malformed(format!("the stage would not open: {e}")))?;

    // Stage metadata: both of these change what the geometry *means*, not just how it looks.
    let meters_per_unit = stage
        .stage_metadata("metersPerUnit")
        .ok()
        .flatten()
        .and_then(|v| f64::try_from(v).ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0) as f32;
    let z_up = stage
        .stage_metadata("upAxis")
        .ok()
        .flatten()
        .and_then(|v| openusd::tf::Token::try_from(v).ok())
        .map(|t| t.to_string().eq_ignore_ascii_case("Z"))
        .unwrap_or(false);

    // Collect first, convert second: `traverse` hands out paths through a closure, and doing the
    // work inside it would borrow the stage while the walk still holds it.
    let mut paths = Vec::new();
    stage
        .traverse(PrimPredicate::DEFAULT, |prim_path| {
            paths.push(prim_path.clone());
        })
        .map_err(|e| malformed(format!("the stage would not traverse: {e}")))?;

    let mut scene = Scene::new();
    let mut meshes = 0usize;
    let mut skipped = 0usize;

    for prim_path in &paths {
        let Ok(Some(mesh)) = Mesh::get(&stage, prim_path.clone()) else {
            continue;
        };
        match convert(&stage, &mesh, meters_per_unit, z_up) {
            Some(trimesh) => {
                scene.push_root(Node::with_payload(
                    prim_path.to_string(),
                    Payload::TriMesh(trimesh),
                ));
                meshes += 1;
            }
            None => skipped += 1,
        }
    }

    if meshes == 0 {
        return Err(malformed(if skipped > 0 {
            "the stage has meshes, but none of them carry usable points and faces".to_string()
        } else {
            format!(
                "the stage composed to {} prims and none of them is a mesh",
                paths.len()
            )
        }));
    }

    scene.set_meta(
        "format",
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("usda") => "OpenUSD (ASCII)",
            Some(e) if e.eq_ignore_ascii_case("usdz") => "OpenUSD (package)",
            Some(e) if e.eq_ignore_ascii_case("usdc") => "OpenUSD (crate)",
            _ => "OpenUSD",
        },
    );
    scene.set_meta("prims", paths.len().to_string());
    scene.set_meta("meshes", meshes.to_string());
    scene.set_meta("up axis", if z_up { "Z" } else { "Y" });
    if (meters_per_unit - 1.0).abs() > 1e-9 {
        scene.set_meta("metres per unit", format!("{meters_per_unit}"));
    }
    if skipped > 0 {
        scene.set_meta("skipped meshes", skipped.to_string());
    }
    Ok(scene)
}

/// One `UsdGeomMesh` into one [`TriMesh`], in world space and in metres.
fn convert(stage: &Stage, mesh: &Mesh, meters_per_unit: f32, z_up: bool) -> Option<TriMesh> {
    let points: Vec<openusd::gf::Vec3f> = mesh.points_attr().get().ok()??;
    if points.is_empty() || points.len() > MAX_POINTS {
        return None;
    }
    // Index arrays come back as a raw `Value`: the crate has no `TryFrom<Value> for Vec<i32>`,
    // and USD writers legitimately author these as either `int[]` or `int64[]`.
    let counts = int_array(&mesh.face_vertex_counts_attr())?;
    let indices = int_array(&mesh.face_vertex_indices_attr())?;
    if counts.is_empty() || indices.is_empty() {
        return None;
    }

    let world = world_transform(stage, mesh);
    let normals: Option<Vec<openusd::gf::Vec3f>> = mesh.normals_attr().get().ok().flatten();
    let has_normals = normals.as_ref().is_some_and(|n| n.len() == points.len());

    let mut bounds = Aabb::EMPTY;
    let mut positions = Vec::with_capacity(points.len() * 12);
    let mut normal_bytes = Vec::with_capacity(points.len() * 12);
    for (i, p) in points.iter().enumerate() {
        let transformed = transform_point(&world, [p.x as f64, p.y as f64, p.z as f64]);
        let scaled = orient(
            [
                transformed[0] as f32 * meters_per_unit,
                transformed[1] as f32 * meters_per_unit,
                transformed[2] as f32 * meters_per_unit,
            ],
            z_up,
        );
        bounds.expand(scaled);
        for c in scaled {
            positions.extend_from_slice(&c.to_le_bytes());
        }
        if has_normals {
            let n = &normals.as_ref().unwrap()[i];
            // Normals rotate with the geometry but do not scale or translate.
            let rotated = transform_direction(&world, [n.x as f64, n.y as f64, n.z as f64]);
            let oriented = orient(
                [rotated[0] as f32, rotated[1] as f32, rotated[2] as f32],
                z_up,
            );
            for c in oriented {
                normal_bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
    }

    // Triangulate. `faceVertexCounts` is per face and quads dominate; a fan is correct for the
    // convex faces USD assets overwhelmingly use, and does not make a non-convex face worse than
    // any other trivial triangulation would.
    let mut triangles: Vec<u32> = Vec::with_capacity(indices.len());
    let mut cursor = 0usize;
    for count in &counts {
        let n = (*count).max(0) as usize;
        if cursor + n > indices.len() {
            break;
        }
        let face = &indices[cursor..cursor + n];
        cursor += n;
        for k in 1..n.saturating_sub(1) {
            let corner = [face[0], face[k], face[k + 1]];
            // An out-of-range index is a corrupt asset, not a triangle.
            if corner
                .iter()
                .all(|i| *i >= 0 && (*i as usize) < points.len())
            {
                triangles.extend(corner.iter().map(|i| *i as u32));
            }
        }
    }
    if triangles.is_empty() {
        return None;
    }

    let mut attributes = vec![Attribute::new(
        Semantic::Position,
        ComponentType::F32,
        3,
        points.len(),
        positions,
    )
    .ok()?];
    if has_normals {
        attributes.push(
            Attribute::new(
                Semantic::Normal,
                ComponentType::F32,
                3,
                points.len(),
                normal_bytes,
            )
            .ok()?,
        );
    }

    Some(TriMesh {
        attributes,
        indices: Indices::U32(triangles),
        bounds,
        material: None,
    })
}

/// Read an integer array, whatever width it was authored at.
fn int_array(attribute: &openusd::usd::Attribute) -> Option<Vec<i64>> {
    use openusd::sdf::Value;
    match attribute.get::<Value>().ok()?? {
        Value::IntVec(v) => Some(v.into_iter().map(i64::from).collect()),
        Value::UintVec(v) => Some(v.into_iter().map(i64::from).collect()),
        Value::Int64Vec(v) => Some(v),
        Value::Uint64Vec(v) => Some(v.into_iter().map(|n| n as i64).collect()),
        _ => None,
    }
}

/// Compose the transform from the stage root down to this prim.
///
/// `local_to_parent_transform` gives one level; walking to the root and multiplying gives world
/// space. A prim that resets the stack stops the walk, which is what `!resetXformStack!` means.
fn world_transform(stage: &Stage, mesh: &Mesh) -> openusd::gf::Matrix4d {
    let time = openusd::usd::TimeCode::new(0.0);
    let mut matrix = mesh
        .local_to_parent_transform(time)
        .unwrap_or(openusd::gf::Matrix4d::IDENTITY);

    if mesh.resets_xform_stack().unwrap_or(false) {
        return matrix;
    }

    let mut path = mesh.prim().path().clone();
    // Bounded: a USD namespace is deep but not unbounded, and a malformed path must not spin.
    for _ in 0..64 {
        let Some(parent) = path.parent() else {
            break;
        };
        if parent.is_abs_root() {
            break;
        }
        // Only an `Xformable` carries a transform. A plain `Scope` in the chain contributes
        // nothing, which is exactly what USD says it should.
        if let Ok(Some(xform)) = Xform::get(stage, parent.clone()) {
            let local = xform
                .local_to_parent_transform(time)
                .unwrap_or(openusd::gf::Matrix4d::IDENTITY);
            matrix = local * matrix;
            if xform.resets_xform_stack().unwrap_or(false) {
                break;
            }
        }
        path = parent;
    }
    matrix
}

/// USD matrices are row-vector convention: a point is a row multiplied on the left.
fn transform_point(m: &openusd::gf::Matrix4d, p: [f64; 3]) -> [f64; 3] {
    let e = &m.0;
    [
        p[0] * e[0] + p[1] * e[4] + p[2] * e[8] + e[12],
        p[0] * e[1] + p[1] * e[5] + p[2] * e[9] + e[13],
        p[0] * e[2] + p[1] * e[6] + p[2] * e[10] + e[14],
    ]
}

fn transform_direction(m: &openusd::gf::Matrix4d, v: [f64; 3]) -> [f64; 3] {
    let e = &m.0;
    let out = [
        v[0] * e[0] + v[1] * e[4] + v[2] * e[8],
        v[0] * e[1] + v[1] * e[5] + v[2] * e[9],
        v[0] * e[2] + v[1] * e[6] + v[2] * e[10],
    ];
    let length = (out[0] * out[0] + out[1] * out[1] + out[2] * out[2]).sqrt();
    if length < 1e-12 {
        [0.0, 1.0, 0.0]
    } else {
        [out[0] / length, out[1] / length, out[2] / length]
    }
}

/// Rotate a Z-up asset into the renderer's Y-up world.
///
/// The renderer, like glTF, is Y-up. A Z-up asset shown without this lies on its back, which
/// reads as a broken import rather than as a convention mismatch.
fn orient(p: [f32; 3], z_up: bool) -> [f32; 3] {
    if z_up {
        [p[0], p[2], -p[1]]
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a `.usda` to a temporary file and remove it afterwards.
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str, text: &str) -> Temp {
            let path = std::env::temp_dir()
                .join(format!("viewer-usd-{}-{name}", std::process::id()));
            std::fs::write(&path, text).expect("the temp directory should be writable");
            Temp(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// One quad, in whatever units and orientation are asked for.
    fn quad(meters_per_unit: f64, up_axis: &str, extra: &str) -> String {
        format!(
            r#"#usda 1.0
(
    metersPerUnit = {meters_per_unit}
    upAxis = "{up_axis}"
)

def Xform "World"
{{
    {extra}
    def Mesh "Plate"
    {{
        int[] faceVertexCounts = [4]
        int[] faceVertexIndices = [0, 1, 2, 3]
        point3f[] points = [(0, 0, 0), (100, 0, 0), (100, 100, 0), (0, 100, 0)]
    }}
}}
"#
        )
    }

    fn mesh_of(scene: &Scene) -> &TriMesh {
        scene
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Some(Payload::TriMesh(m)) => Some(m),
                _ => None,
            })
            .expect("expected a mesh")
    }

    #[test]
    fn a_usda_quad_reads_and_triangulates() {
        let file = Temp::new("quad.usda", &quad(1.0, "Y", ""));
        let scene = import(&file.0).expect("a valid usda stage");
        let m = mesh_of(&scene);
        // One quad becomes two triangles.
        assert_eq!(m.triangle_count(), 2);
        assert_eq!(m.vertex_count(), 4);
    }

    #[test]
    fn meters_per_unit_scales_the_geometry() {
        // The same 100-unit quad in centimetres is one metre across, not a hundred.
        let file = Temp::new("cm.usda", &quad(0.01, "Y", ""));
        let scene = import(&file.0).unwrap();
        let width = mesh_of(&scene).bounds.extent()[0];
        assert!((width - 1.0).abs() < 1e-4, "{width}");
    }

    #[test]
    fn a_z_up_stage_is_rotated_into_the_renderers_y_up_world() {
        // The quad lies in the XY plane. Y-up leaves it edge-on; Z-up makes it the ground plane,
        // so its vertical extent has to collapse and its depth has to open up.
        let flat = Temp::new("y.usda", &quad(1.0, "Y", ""));
        let ground = Temp::new("z.usda", &quad(1.0, "Z", ""));
        let y = mesh_of(&import(&flat.0).unwrap()).bounds.extent();
        let z = mesh_of(&import(&ground.0).unwrap()).bounds.extent();
        assert!(y[1] > 1.0 && y[2] < 1e-4, "Y-up: {y:?}");
        assert!(z[1] < 1e-4 && z[2] > 1.0, "Z-up: {z:?}");
    }

    #[test]
    fn an_ancestor_transform_is_composed_into_world_space() {
        let file = Temp::new(
            "xform.usda",
            &quad(1.0, "Y", "double3 xformOp:translate = (1000, 0, 0)\n    uniform token[] xformOpOrder = [\"xformOp:translate\"]"),
        );
        let scene = import(&file.0).unwrap();
        let b = mesh_of(&scene).bounds;
        assert!(b.min[0] >= 1000.0 - 1e-3, "the parent translate was lost: {b:?}");
    }

    #[test]
    fn a_stage_with_no_meshes_says_so_rather_than_opening_empty() {
        let file = Temp::new(
            "empty.usda",
            "#usda 1.0\n\ndef Xform \"World\"\n{\n    def Scope \"Looks\"\n    {\n    }\n}\n",
        );
        let error = import(&file.0).unwrap_err().to_string();
        assert!(error.contains("mesh"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_usd_is_refused() {
        let file = Temp::new("nope.usda", "this is not a USD stage");
        assert!(import(&file.0).is_err());
        assert!(import(Path::new("/definitely/not/here.usda")).is_err());
    }

    #[test]
    fn out_of_range_face_indices_are_dropped_not_drawn() {
        let model = r#"#usda 1.0

def Mesh "Broken"
{
    int[] faceVertexCounts = [3, 3]
    int[] faceVertexIndices = [0, 1, 2, 0, 1, 99]
    point3f[] points = [(0, 0, 0), (1, 0, 0), (0, 1, 0)]
}
"#;
        let file = Temp::new("broken.usda", model);
        let scene = import(&file.0).unwrap();
        assert_eq!(mesh_of(&scene).triangle_count(), 1);
    }

    #[test]
    fn a_polygon_with_more_than_four_sides_fans_correctly() {
        let model = r#"#usda 1.0

def Mesh "Pentagon"
{
    int[] faceVertexCounts = [5]
    int[] faceVertexIndices = [0, 1, 2, 3, 4]
    point3f[] points = [(0, 0, 0), (1, 0, 0), (2, 1, 0), (1, 2, 0), (0, 1, 0)]
}
"#;
        let file = Temp::new("pentagon.usda", model);
        let scene = import(&file.0).unwrap();
        // A fan over five corners is three triangles.
        assert_eq!(mesh_of(&scene).triangle_count(), 3);
    }

    #[test]
    fn the_orientation_swap_is_its_own_inverse_in_shape() {
        // Y-up is untouched; Z-up moves the vertical axis and negates what becomes depth.
        assert_eq!(orient([1.0, 2.0, 3.0], false), [1.0, 2.0, 3.0]);
        assert_eq!(orient([1.0, 2.0, 3.0], true), [1.0, 3.0, -2.0]);
    }
}
